# File Size Policy

Enforced by `scripts/check-file-size-budget.sh` (CI job **File Size Ratchet**).
Repo-local: this governs Loom's own source, and is not installed into consumer
repos.

## The rule

**A source file already over 1000 code lines is frozen at its current size. It
may shrink. It may not grow.** Files under the threshold are unconstrained.

That is the whole policy. Nothing has to be refactored up front; the gate fires
only at the moment a change makes a known-oversized file worse.

The debt ledger lives in `scripts/file-size-baseline.txt` — every
currently-over-threshold file with its recorded size. **Those numbers are debt,
not targets. They should only ever go down.**

## Why (#7711)

Loom's source grows monotonically because every individual addition is
defensible while the aggregate is the problem — the same failure mode
`check-claude-md-budget.sh` (#4014) was built to counter.

Measured on 2026-09-15 (`main` @ 38f85147):

- 86 of 228 Rust files exceeded 1000 lines and held **79% of all Rust LOC**.
- The five worst files had reached their **entire** current size within 90 days.
- Add-to-delete ratio across those files was roughly **20:1**.

```
net  +9430  (+10048/-618)   ipc.rs          112 commits in 90d
net  +9949  (+10378/-429)   role_runner.rs   47 commits
net  +9157  (+10432/-1275)  work_finder.rs   73 commits
```

Agents add; they essentially never restructure. A gate is the counter-pressure.

### Why 1000

There is no authoritative Rust file-size rule. The anchors:

- Clippy's proposed [`too_many_lines_in_file`](https://github.com/rust-lang/rust-clippy/issues/16674)
  defaults to **1000**, counting code lines only. The most defensible
  Rust-native number, and what this policy adopts.
- `rustc`'s own `tidy` enforces 3000, but only for non-Rust files.
- The widely-cited "150–500 line sweet spot" is a Python blog post, not
  research, and is punishing for Rust (inline tests, derives, explicit error
  handling).

## What this policy is NOT

**It is not "you touched a large file, now refactor it."** That rule was
considered and rejected:

- **It would fire constantly.** `ipc.rs` is touched more than once a day (112
  commits/90d); `types.rs` 94. That is a tax on nearly every sweep, not
  occasional cleanup — agents would either bloat every PR or quietly ignore it.
- **It couples unrelated refactors into feature PRs.** The mean touch is
  +160/−24 lines; a mandated split turns that into a four-figure diff, which
  makes Judge review worse and buries the actual change.
- **It fights the concurrency model.** Parallel builders work in separate
  worktrees. A module split moves `use` statements and `pub(crate)` visibility
  across the file, conflicting with every other in-flight PR touching it —
  worst exactly when the fleet is busiest.

## Counting rules

Code lines only. Blank lines and comment-only lines never count.

**Rust counts production lines only** — everything before the first
`#[cfg(test)]`. Inline test modules are idiomatic Rust and are **49% of this
repo's Rust bulk** (142,674 of 289,955 lines). Taxing them would push tests out
of the codebase for the wrong reason. Growing a test module is always allowed.

This is why the ledger numbers look smaller than `wc -l`: `role_runner.rs` is
9,949 raw lines but only 2,208 production lines.

## Exemptions

Only two categories are exempt from measurement, both cases where a violation
would be permanently unfixable here:

1. **Vendored** — `defaults/hooks/guard-destructive-generic.sh` (8,872 lines,
   the single largest shell file in the repo) and `guard-destructive.sh`. The
   canonical copies live in [rjwalters/repo](https://github.com/rjwalters/repo)
   and are re-vendored at release time; the file's own header forbids
   hand-editing generic pattern behavior. A local split would be reverted by the
   next re-vendor. Structural fixes belong upstream.
2. **Installed mirrors** — `.loom/hooks/`, `.loom/scripts/`, `.loom/docs/` are
   resync copies of `defaults/`. They are measured at their `defaults/` source;
   measuring both would double-count every violation and churn the baseline on
   every resync commit.
3. **Extracted Rust test modules** — `*/tests.rs` and `*/tests/*.rs`. The policy
   does not tax Rust tests whether they are inline or extracted, and counting an
   extracted module as production would make this gate **fail the very refactor
   it exists to reward**: moving `#[cfg(test)] mod tests` out of `ipc.rs`
   creates a 4,400-line `ipc/tests.rs` that newly "crosses" the threshold, while
   the production code being measured did not change by a single line.

**Bootstrap scripts are deliberately NOT exempt.** `install.sh`,
`scripts/install-loom.sh`, `loom-daemon-update.sh` and `loom-daemon-start.sh`
run before — or manage — the `loom-daemon` binary, so they can never be ported
into it. But "cannot be ported to Rust" is not "may grow without bound"; the
ratchet is exactly the right mechanism for them.

## Hitting the gate

When CI says a file grew:

1. **Preferred:** put the new code in a new sibling module and leave a small
   dispatch/match arm behind. In Rust this is cheap — `mod foo;` plus a new
   file.
2. **Or:** remove at least as much as you added from the same file.

**Do not raise the threshold. Do not hand-edit a baseline number upward.** That
is the ratchet slipping, which is the entire thing this prevents. A diff to
`file-size-baseline.txt` that raises any number, or adds a row without a clear
reason in the PR description, should be treated by Judge as a red flag.

After a real refactor, run `scripts/check-file-size-budget.sh --update` to
record the shrinkage and tighten the ratchet.

## Shell: port to Rust, tiered

There is established precedent for porting shell into `loom-daemon`
subcommands — #4552 (the `loom_tools` script-helper family), #4471
(`agent_spawn.py` / `agent_wait.py`), #4105 / #4108 (`tokens bootstrap` /
`check`). The landing zone is `loom-daemon/src/cli/` or
`loom-daemon/src/script_helpers/`.

Shell totals 207,441 lines across 456 files, but only one tier is a real port
candidate:

| Tier | Size | Action |
|---|---|---|
| Vendored (`guard-destructive*.sh`) | ~9k | Exempt — upstream owns it |
| Bootstrap (`install.sh`, `install-loom.sh`, `loom-daemon-{update,start}.sh`) | ~11k | Ratchet only; never port (chicken-and-egg) |
| Port candidates (`merge-pr.sh`, `worktree.sh`, `claude-wrapper.sh`) | ~9k | Finish the port — all three already have a `loom-daemon forge` delegation ladder, so this is incremental |
| Shell tests (242 files) | ~115k | Split, don't port — bash testing bash is legitimate; cap size only |

Porting also improves hook latency: `PreToolUse` fires on every tool call, and a
binary exec beats parsing a multi-thousand-line bash script. The vendoring
constraint blocks this for the guard specifically.

## Related tracks

Deferred from #7711, each needing its own issue:

1. **Mechanical test extraction** — move `#[cfg(test)] mod tests` to sibling
   files. Compiler-verified, no judgment required, roughly halves the worst
   files. Never coupled to feature work.
2. **Semantic decomposition** of files still over threshold after (1) —
   Architect-proposed, one file at a time, scheduled when that file has no open
   PRs touching it.
3. **Shell port tier 3** — finish `merge-pr.sh` / `worktree.sh` /
   `claude-wrapper.sh` onto native subcommands.

## Maintenance

`scripts/check-file-size-budget.sh --self-test` verifies the gate itself against
synthetic fixtures (counting rules, exemptions, all three verdicts). CI runs it
before the real check, so an edit that silently disarms the measurement fails
loudly instead of reporting OK forever.
