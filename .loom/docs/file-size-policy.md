# File Size Policy

Enforced by `scripts/check-file-size-budget.sh` (CI job **File Size Ratchet**).
Repo-local: this governs Loom's own source, and is not installed into consumer
repos.

> **This document states rules, not measurements.** Current numbers live in
> `scripts/file-size-baseline.txt`, which is generated and therefore never
> stale. The dated measurements that motivated the policy are in #7711. Please
> do not paste counts back into this file — the first revision did, and two of
> them were wrong within a day.

## The rule

**A source file already over 1000 code lines is frozen at its current size. It
may shrink. It may not grow.** Files under the threshold are unconstrained.

That is the whole policy. Nothing has to be refactored up front; the gate fires
only at the moment a change makes a known-oversized file worse.

The debt ledger is `scripts/file-size-baseline.txt` — every
currently-over-threshold file with its recorded size. **It is debt, not a
target. The numbers should only go down, and the list should only get shorter.**

## Why

Large files are hard for an LLM coding agent to work with. Opening one spends
context on everything you did not need, edits land further from the code that
constrains them, and a failed attempt has to re-read the whole thing to try
again. Smaller files mean an agent can hold the entire unit in view, make a
targeted change, and recover cleanly when it gets something wrong.

Loom's source grows monotonically toward the opposite state, because every
individual addition is defensible while the aggregate is the problem — the same
failure mode `check-claude-md-budget.sh` (#4014) was built to counter. Agents
add; they essentially never restructure. A gate is the counter-pressure.

### Why 1000

There is no authoritative Rust file-size rule. The anchors:

- Clippy's proposed [`too_many_lines_in_file`](https://github.com/rust-lang/rust-clippy/issues/16674)
  defaults to 1000, counting code lines only. The most defensible Rust-native
  number, and what this policy adopts.
- `rustc`'s own `tidy` enforces 3000, but only for non-Rust files.
- The widely-cited "150–500 line sweet spot" is a Python blog post, not
  research, and is punishing for Rust.

## What this policy is NOT

**It is not "you touched a large file, now refactor it."** That rule was
considered and rejected:

- **It would fire constantly.** The hottest files here are touched more than
  once a day. That is a tax on nearly every sweep, not occasional cleanup —
  agents would either bloat every PR or quietly ignore it.
- **It couples unrelated refactors into feature PRs.** A mandated split turns a
  small touch into a four-figure diff, which makes Judge review worse and buries
  the actual change.
- **It fights the concurrency model.** Parallel builders work in separate
  worktrees. A module split moves `use` statements and `pub(crate)` visibility
  across the file, conflicting with every other in-flight PR touching it — worst
  exactly when the fleet is busiest.

## Counting rules

Code lines only. Blank lines and comment-only lines never count.

**The whole file counts**, including inline `#[cfg(test)]` modules and test files
in any language. A big file is hard to work with whatever is in it, and an agent
editing the production half still pays for the test half sitting in the same
buffer.

Extracting a test module to a sibling file is therefore a real improvement, and
the ratchet records it as one: the parent shrinks, and the extracted file is
measured on its own terms. If the extracted file is itself over threshold, it is
still too big — that is the correct signal, not a false positive.

> An earlier revision tried to count production lines only, tracking
> `#[cfg(test)]` to exclude test modules. It produced two silent bugs in opposite
> directions — one hiding oversized files from the gate, one failing honest
> changes — before being abandoned. Counting the whole file needs none of that
> machinery. Prefer the rule you cannot implement incorrectly.

## Exemptions

Only cases where a violation would be permanently unfixable here:

1. **Vendored** — `defaults/hooks/guard-destructive*.sh`. The canonical copies
   live in [rjwalters/repo](https://github.com/rjwalters/repo) and are
   re-vendored at release time; the file's own header forbids hand-editing
   generic pattern behavior. A local split would be reverted by the next
   re-vendor. Structural fixes belong upstream.
2. **Installed mirrors** — `.loom/hooks/`, `.loom/scripts/`, `.loom/docs/` are
   resync copies of `defaults/`, measured at their `defaults/` source.

**Bootstrap scripts are deliberately NOT exempt.** `install.sh`,
`scripts/install-loom.sh`, `loom-daemon-update.sh` and `loom-daemon-start.sh`
run before — or manage — the `loom-daemon` binary, so they can never be ported
into it. But "cannot be ported to Rust" is not "may grow without bound".

**Test files are not exempt** in any language.

## Hitting the gate

When CI says a file grew:

1. **Preferred:** put the new code in a new sibling module and leave a small
   dispatch/match arm behind. In Rust this is cheap — `mod foo;` plus a new
   file.
2. **Or:** remove at least as much as you added from the same file.
3. **If the change legitimately splits a large file into smaller ones**, run
   `scripts/check-file-size-budget.sh --update` and say so in the PR
   description. The ledger total should drop.

**Do not raise the threshold. Do not hand-edit a baseline number upward.** That
is the ratchet slipping, which is the entire thing this prevents. A baseline
diff that raises a number, or grows the total, without a stated reason should be
treated by Judge as a red flag.

## When the baseline goes stale under you

CI evaluates the **merge result**, not your branch in isolation. So if an
over-threshold file grows on `main` after your baseline snapshot was taken, your
PR fails for growth it did not cause.

This is mostly a one-time bootstrap artifact: once the gate is live, growth is
caught on the PR that causes it. The residual case is two PRs in flight — A
grows a file and merges, B was opened earlier and now fails. The remedy is the
same either way:

```
git rebase origin/main
scripts/check-file-size-budget.sh --update
```

and **say in the PR description that the raised number came from `main`, not
from your change**. This is the one legitimate reason for a baseline number to
go up, and it should be visibly justified every time, because the policy
otherwise treats an upward edit as the ratchet slipping.

## Mechanical refactors: use the compiler's tooling, never text surgery

When moving code — extracting a test module, splitting a file — **move the text
verbatim and let `rustfmt` re-indent it.** Do not hand-transform indentation.

This is not style advice. A dedent implemented as "strip four spaces from every
line" is a *lexical* operation applied to a *syntactic* structure: it cannot
tell code from the inside of a raw string literal, so it silently rewrites
embedded shell scripts, JSON fixtures and config blobs. That bug occurred once
during #7718 and was invisible in the diff, invisible to the compiler, and
invisible to the test suite, because the corrupted fixtures happened to be
whitespace-insensitive. `rustfmt` has a real Rust lexer, knows exactly where
literals begin and end, and is already enforced in CI.

**Acceptance test for any mechanical move:** every string literal is
byte-identical before and after, checked by something that lexes the language —
not by reading the diff. Verbatim move plus `cargo fmt` makes that true by
construction.

## Shell: port to Rust, tiered

There is established precedent for porting shell into `loom-daemon` subcommands
— #4552 (the `loom_tools` script-helper family), #4471 (`agent_spawn.py` /
`agent_wait.py`), #4105 / #4108 (`tokens bootstrap` / `check`). The landing zone
is `loom-daemon/src/cli/` or `loom-daemon/src/script_helpers/`.

| Tier | Action |
|---|---|
| Vendored (`guard-destructive*.sh`) | Exempt here — [rjwalters/repo](https://github.com/rjwalters/repo) owns the structure. Rewriting it is an upstream proposal (#7760), not a Loom change. |
| Bootstrap (`install.sh`, `install-loom.sh`, `loom-daemon-{update,start}.sh`) | Ratchet. **How much must stay shell is unresolved** — `auto_update.rs` already implements fetch/rebuild/restart in Rust, so the earlier "never port, chicken-and-egg" claim is being re-derived in #7758. |
| Port candidates (`merge-pr.sh`, `worktree.sh`, `claude-wrapper.sh`) | Finish the port — all three already have a `loom-daemon forge` delegation ladder, so this is incremental |
| Shell tests | Split (#7741). Note these are shell **because the code under test is shell** — that is a consequence of the implementation language, not an independent reason to keep either. |

Porting also improves hook latency: `PreToolUse` fires on every tool call, and a
binary exec beats parsing a multi-thousand-line bash script. The vendoring
constraint blocks this for the guard specifically.

## Related tracks

- **#7718** — extract inline `#[cfg(test)]` modules to sibling files.
- **#7716** / **#7725** — the same ratchet, applied to agent-facing markdown
  (role prompts and slash commands), measured in tokens rather than lines. See
  ["Markdown token budget"](#markdown-token-budget) below.
- Semantic decomposition of what remains over threshold: Architect-proposed, one
  file at a time, scheduled when that file has no open PRs touching it.

## Maintenance

`scripts/check-file-size-budget.sh --self-test` verifies the gate itself against
synthetic fixtures. CI runs it before the real check, so an edit that silently
disarms the measurement fails loudly instead of reporting OK forever. Both bugs
found in the first revision reported a green gate.

## Markdown token budget

Enforced by `scripts/check-markdown-token-budget.sh` (CI job **Markdown Token
Ratchet**). Same ratchet mechanism as the source-file gate above — read that
section first — applied to a different category of file, for a different
reason (#7716, #7725).

**Why a separate gate instead of extending the one above**: role prompts and
slash-command bodies are not source code. They are prepended into an agent's
context on *every* invocation — the same load-bearing category
`check-claude-md-budget.sh` (#4014) already guards for `CLAUDE.md` alone —
and there is no code-line/comment-line distinction to make in prose. A single
gate that tried to cover both would need two counting rules and two
exemption lists; two small gates are easier to reason about than one gate
with a branch in the middle.

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
mirror generally, for the same reason the source-file gate exempts
`.loom/hooks`, `.loom/scripts`, and `.loom/docs`.

**Baseline.** Unlike `scripts/file-size-baseline.txt` (which lists only
files already over threshold), `scripts/markdown-token-baseline.txt` records
**every** file in the measured set, unconditionally — the set is small and
fully enumerable, so tracking all of it costs nothing and catches growth in
any file, not just the biggest ones. `--threshold` still exists for parity
with the source-file gate's flag surface, and matters only for a file that
is not yet in the baseline (a brand-new slash command): if that file is
already over threshold on arrival, the gate treats it the same as a source
file newly crossing 1000 lines.

Same rules for hitting the gate, the same `--update` discipline, and the
same stale-baseline-under-you remedy as the source-file gate above — nothing
about those parts changes for markdown.
