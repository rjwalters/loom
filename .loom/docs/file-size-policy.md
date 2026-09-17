# File Size Policy

Enforced by `scripts/check-file-size-budget.sh` (CI job **File Size Ratchet**).
Repo-local: this governs Loom's own source, and is not installed into consumer
repos.

> **This document states rules, not measurements.** Current numbers live in
> `scripts/file-size-baseline.txt`, which is generated and therefore never
> stale. The dated measurements that motivated the policy are in #7711. Please
> do not paste counts back into this file — the first revision did, and two of
> them were wrong within a day.
>
> One deliberate exception: the shell-migration **floor** in
> ["Shell: port to Rust, tiered"](#shell-port-to-rust-tiered) is stated as a
> line count, because a ratchet with no floor cannot distinguish progress from
> stasis (#7758). Those figures are dated and approximate by construction; the
> baseline file still wins on anything it measures.

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

**Bootstrap is a responsibility, not a file-level exemption.** Obtaining a
runnable `loom-daemon` may require shell; manifest processing, ownership
decisions and reinstall policy *after* that point do not — that is
[ADR-0018](../../docs/adr/0018-rust-owns-behavior-shell-reaches-it.md), and
epic #7810 is the migration that follows from it. The irreducible core is
therefore not "the big install-ish scripts" but exactly the files that must run
on a machine where the binary is absent, unbuilt, or being removed:

| script | why it cannot be ported into the binary |
|---|---|
| `install.sh` | Runs where no Loom artifact, and possibly no toolchain, exists. |
| `scripts/install-loom.sh` | Same, plus registering the MCP server at user scope. |
| `scripts/uninstall-loom.sh` | Must still work once the binary is gone — it must never depend on the thing it removes. |
| `defaults/scripts/lib/locate-daemon-bin.sh` | Executable discovery: pre-binary by definition. Shared by 30 stubs, and the sanctioned shell job under ADR-0018. |

Together ≈4,214 code lines. **They are ratcheted like everything else** — they
may shrink, they may not grow. "Cannot be ported to Rust" is not "may grow
without bound".

The earlier version of this section also named `loom-daemon-update.sh` and
`loom-daemon-start.sh` as unportable. That was inherited, not derived, and it is
wrong in both directions: those two are port candidates, and
`uninstall-loom.sh` and `locate-daemon-bin.sh` — which it did not name — are the
ones that genuinely must stay shell (#7758).

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

## Mechanical refactors: use the language's own tooling, never text surgery

When moving text — extracting a test module, splitting a file, dedenting,
bulk-renaming — **move it verbatim and let the language's own formatter or
parser do the reshaping.** Do not hand-transform indentation or patch by regex.

This is not style advice. A line-oriented transform is a *lexical* operation
applied to a *syntactic* structure: it cannot tell code from the inside of a
string literal, from a comment, or from a markdown heading's depth. The failures
are silent and land in the minority case:

- **Rust** (#7718) — a dedent implemented as "strip four spaces from every line"
  rewrote the interiors of raw string literals, silently corrupting embedded
  shell scripts and JSON fixtures in 4 of 8 files. Invisible in the diff,
  invisible to the compiler, and invisible to the test suite, because the
  corrupted fixtures happened to be whitespace-insensitive.
- **Shell** (#7741) — a column-0 `^VAR=` regex treated continuation lines inside
  multi-line quoted test payloads as assignments and hoisted 16 of them out of
  the middle of test strings.
- **Counting, in any language** (#7751, #7766) — grepping for a construct
  matched *comments naming* that construct, inflating an at-risk survey from 8
  files to 13 and reporting a removal as incomplete when code-only it was zero.
  Exclude comment lines before reporting any count, and say "code-only" in the
  report.
- **Markdown** (#7793) — a heading edit anchored on `## …` matched inside a
  `### …` heading (`##` is a substring of `###`), ate one `#`, and silently
  demoted a section. Its guard, `assert body.count(old) == 1`, *passed*:
  counting occurrences does not detect matching the wrong kind of text.

**Acceptance test for any mechanical move:** an invariant that must survive the
move is byte-identical before and after, checked by something that parses the
relevant syntax — not by reading the diff and not by counting matches. The tool
differs by language; the rule does not:

| Language | Invariant to assert | Tool that can see it |
|---|---|---|
| Rust | every string literal unchanged | verbatim move + `cargo fmt` (real lexer, already in CI) |
| Shell | every quoted payload/heredoc unchanged | `shfmt -d`, `shellcheck` |
| Markdown | heading depths and the full heading list unchanged | dump every heading before/after and diff the lists |

Verbatim move plus the language's formatter makes that true by construction.
Full recipe, plus the four other verification classes from #7793 (postcondition
checks, CI-state enumeration, `--body-file`, falsifying-fact checks):
[`verification-recipes.md`](verification-recipes.md).

## Shell: port to Rust, tiered

There is established precedent for porting shell into `loom-daemon` subcommands
— #4552 (the `loom_tools` script-helper family), #4471 (`agent_spawn.py` /
`agent_wait.py`), #4105 / #4108 (`tokens bootstrap` / `check`). The landing zone
is `loom-daemon/src/cli/` or `loom-daemon/src/script_helpers/`.

The tiers below are the categorical verdicts derived in **#7758** and ruled on
2026-09-16. [ADR-0018](../../docs/adr/0018-rust-owns-behavior-shell-reaches-it.md)
(#7777, accepted) is the principle they implement; epic **#7810** owns the
execution order. A tier here says *what may be ported*, never *when* —
sequencing belongs to #7810, phase by phase. These tiers describe shell that
already exists; for what language a **new** file may be written in, see
[`shell-language-policy.md`](shell-language-policy.md) (#7762), whose
categories are these same #7758 verdicts.

| Tier | Count | Verdict | Why |
|---|---|---|---|
| **Vendored** — `guard-destructive*.sh`, `resync-installed.sh`, +2 | 5 | Stays shell, **upstream** | The canonical copy lives in [rjwalters/repo](https://github.com/rjwalters/repo) and is re-vendored at release, so a local split would be reverted by the next re-vendor. Restructuring is an upstream proposal (#7760), not a Loom change. |
| **Hook entry points** — `guard-destructive.sh`, `guard-loom-workflow.sh`, `guard-worktree-paths.sh`, `skill-router.sh`, `guard-background-subagents.sh` | 5 | The **stub** stays shell; the logic under it is a port candidate | `PreToolUse` / `SessionStart` need a shell-invocable command. Only the invocation is irreducible — everything below it can move. (These currently fail **open** when absent: #7761.) #7758's inventory listed a sixth, `session-start-handoff.sh`; this repo does not track that file — `.claude/settings.json` invokes it from `.claude/skills/repo/hooks/`, so it is Repo Skills-owned, upstream of Loom. A file outside this tree cannot sit in a tier of this tree's shell inventory, so the count is 5 (#7903). |
| **Bootstrap core** — the four files in ["Exemptions"](#exemptions), plus `sync-labels.sh` | 5 | Stays shell, **ratcheted** | Runs before a binary is guaranteed, or after it is gone. `sync-labels.sh` (587) is pure forge CRUD that would be natural Rust, but it runs standalone at install time and that constraint wins. |
| **Port candidates** | 12 named | Port — see the per-file table below | Real logic, with Rust already adjacent in most cases. |
| **Already thin stubs** (<40 code lines over `loom-daemon`) | 28 | **Done** | 571 lines total, averaging 20 each. This is the target state, and a large slice of the surface has already reached it. |
| **Shell tests** | 251 suites | **Follow their subject** | They are shell *because the code under test is shell*. That is a consequence of the implementation language, not an independent argument for keeping either, and must not be cited as one (#7755). Split oversized ones (#7741); they port when their subject ports. |

The ~180 smaller production scripts not named anywhere here are governed by the
categorical rules above; #7810 deliberately does not wait on a complete
per-file inventory.

### Port candidates, and the contract each port must retire

A port is not finished when the Rust exists — it is finished when the
shell-era interface it replaced is gone. The last column is what makes each of
these an API change rather than a rewrite.

| script | code | Rust today | contract a port must retire |
|---|---|---|---|
| `cli/loom-daemon-update.sh` | 1,770 | `auto_update.rs` **schedules and decides**, and delegates release resolution / fetch / verify / provision **to this script** | `--resolve-json` (read-only decision info) and `--no-restart` (the rebuild), both consumed by `auto_update.rs` — an API change on both sides (#7810 Phase 5–6) |
| `cli/loom-daemon-start.sh` | 1,185 | `daemon_service.rs`, `restart_verify.rs` | launchd/systemd unit management; supervisor handoff |
| `cli/loom-daemon-watchdog.sh` | ~990 | `main_health_gate.rs`, `health.rs` | poll cadence and restart policy |
| `claude-wrapper.sh` | 1,675 | none — retry/backoff lives *only* here | the retry policy itself, which is application logic (#7810 Phase 7–8) |
| `worktree.sh` | ~1,820 | `worktree_ops/`, `worktree_reaper.rs` | the `.loom-managed` sentinel contract; also forge-blind today (#7765) |
| `merge-pr.sh` | ~1,460 | `forge_cmd.rs` (`loom-daemon forge auto-merge`) | already has a delegation ladder — the most incremental port available |
| `resync-installed.sh` | ~1,170 | `init/`, `daemon_install_state.rs` | vendored **and** a port candidate; resolve the upstream question first |
| `lib/forge-helpers.sh` | 1,072 | forge dispatch already in the daemon | none of its own — it evaporates as its callers port, so it follows its consumers |
| `check-main-clean.sh` | 489 | — (git-state inspection) | the build-gate invocation name, which stays behind as a stub |
| ~~`classify-dependency-block.sh`~~ | ~470 → 7 | `dep_classify/` (#7952) | **Done.** Ported with its two sourced helpers (`detect-dependency-cycle.sh`, `detect-startable-subset.sh`) — one unit, since `classify` sourced both. All three names stay as thin stubs: role prompts invoke them by path and parse their stdout line-wise |
| ~~`dep-recheck-fingerprint.sh`~~ | 390 → 7 | `dep_recheck/` (#7961) | **Done.** All five subcommands ported; the name stays as a thin stub because `curator.md` invokes it by path and `eval`s its `KEY=VALUE` output |
| `claim-staleness.sh` | 280 | — | a pure decision function over forge state |

Counts are code lines as derived in #7758 on 2026-09-16 and drift within days;
`scripts/file-size-baseline.txt` is authoritative for anything over threshold.

### The floor the ratchet tracks against

What is expected to still be shell when the migration is complete:

| component | code lines |
|---|---|
| Bootstrap core (4 files) | ≈4,214 |
| `sync-labels.sh` | 587 |
| 6 hook entry points | the stub only — tens of lines each, not today's sizes |
| 5 vendored files | owned upstream; not Loom's to shrink |
| **floor** | **≈4,800 + stubs** |

Everything else under `defaults/scripts/` is portable in principle. That figure
is the floor #7711's ratchet has to track against: the shell total should
approach it and then stop. Without a floor, a shrinking number and a stalled one
look identical.

Porting also improves hook latency: `PreToolUse` fires on every tool call, and a
binary exec beats parsing a multi-thousand-line bash script. The vendoring
constraint blocks this for the guard specifically.

## Related tracks

- **#7718** — extract inline `#[cfg(test)]` modules to sibling files.
- **#7716** / **#7725** — the same ratchet, applied to agent-facing markdown
  (role prompts and slash commands), measured in tokens rather than lines. See
  ["Markdown token budget"](#markdown-token-budget) below.
- **#8053** — the same ratchet again, applied per *role* to the whole prompt
  prefix one spawned session injects, which the per-file gate cannot see. See
  ["Per-role prompt prefix budget"](#per-role-prompt-prefix-budget) below.
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

## Per-role prompt prefix budget

Enforced by `scripts/check-role-prompt-budget.sh` (CI job **Role Prompt Prefix
Ratchet**). Third instance of the same ratchet, on the one quantity the other
two cannot see: the **sum** of everything a single spawned role session injects
(#8053).

**Why the per-file gate above is not enough.** A per-file ratchet is blind to
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
and pass forever. Same exemption, same reason, as the two gates above.

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
role's set. `--update` discipline is unchanged from the gates above.
