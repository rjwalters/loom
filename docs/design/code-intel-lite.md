# A lightweight code-graph / blast-radius helper for Judge and Hermit — evaluation

**Type**: Design evaluation / build-vs-integrate recommendation
**Status**: Complete (evaluation only — no runtime, role, hook, script, or config change ships with it)
**Source issue**: [#5848](https://github.com/rjwalters/loom/issues/5848)
**Upstream studied**: [damusix/atomic-claude](https://github.com/damusix/atomic-claude) (MIT) code-intel
engine, read as **read-only research only**. Nothing from it is vendored, copied, or depended on.
**Prior art in this repo**: [`docs/research/atomic-claude-evaluation.md`](../research/atomic-claude-evaluation.md)
§2 (the "adapt" verdict this issue was filed from) and
[`docs/notes/prometheus-comparison.md`](../notes/prometheus-comparison.md) §Q1 / "Spike candidate #2"
(an independently-derived, near-identical proposal, already scope-guarded).
**Measurements**: taken on this repo at `090d15e0`, 2026-08-09. Commands shown so they can be re-run.

## TL;DR

**Recommendation: do not build or integrate a code graph or a symbol index — not now, and not on
the strength of the current evidence.** All three surveyed alternatives lose on the same two facts,
both measured below:

1. **There is no latency problem to solve.** A whole-repo, no-match `ripgrep` scan of this
   repository finishes in **~11 ms**. An index cannot make a query that already costs 11 ms
   meaningfully faster. The only thing an index could buy is *precision*, not speed.
2. **A Rust+TS call graph would miss most of Loom's actual blast radius.** Loom's coupling runs
   through shell scripts, role prompts, and Markdown at least as much as through compiled code. For
   the Rust symbol `dispatch_sweep`, **17 of the 52 files that reference it are `.rs` (33%)** — the
   other 35 are Markdown, shell, TypeScript, and JSON. A graph over Rust + TypeScript would
   confidently report "these are the callers" while being blind to two thirds of the reference
   surface. **A confident wrong answer is worse than grep**, which at least sees all of it.

What *is* worth building is much smaller and is not a code graph: a **stateless, whole-repo
reference-evidence helper** that takes a symbol (or the symbols a diff removed/renamed) and reports
references bucketed by file class, over **all tracked files** — no index, no database, no new binary
dependency, no state that outlives the command. Sketch in §6. It is proposed as a candidate, gated
on a measurement (§8), not as an approved build.

| Question the issue asked | Answer |
|---|---|
| Survey of 2–3 concrete alternatives | §4 — ast-grep, LSP call hierarchy, purpose-built tree-sitter map (+ ctags as a fourth, briefly) |
| Build vs. integrate | **Neither, for a graph/index.** Optionally build the stateless helper in §6 |
| Scope: Rust+TS only, or broader? | §7 — **broader, and inverted**: structural parsing scoped to Rust+TS is the *wrong* axis; the evidence rule must cover **every tracked file** including `.sh`, `.md`, `.yml`, `.json` |
| Degradation contract | §5 — absent tool never blocks, never errors to the forge, never decides a verdict |

## 1. The two consumers, stated precisely

The issue names two roles. Their questions look similar and are not.

### Judge — "does this diff's blast radius match what changed?"

Concretely, and narrower than it first sounds: **for each symbol the diff removes, renames, or
changes the signature of, are there references outside the diff that the diff did not update?**

The key property is that **the diff names the symbols**. Judge does not need a whole-repo graph; it
needs reverse lookup for `O(symbols touched by this diff)` names — typically a handful. This is a
bounded query, not a standing index question. It slots into
[`judge.md`](../../defaults/.claude/commands/loom/judge.md) §"Evaluation Focus Areas → Correctness"
and complements the already-shipped changed-crate test scoping in
[`judge-reference.md`](../../defaults/.claude/commands/loom/judge-reference.md) §"Rust Repositories",
which already derives "what changed" from the diff for a different purpose.

Judge's binding constraint is time. `judge.md` §"Time budget — do not hang (#3910)" requires the
whole review to complete "in minutes", warns that a hung Judge "silently wedges the whole sweep's
back half", and requires every long-running command to carry an explicit timeout. Anything that
costs a build-class workload inside a Judge pass is disqualified on that ground alone.

Judge's failure mode is **false negatives**: a missed external reference means an approved PR that
breaks a caller. A false positive (a hit in a comment) costs Judge a few seconds of reading.

### Hermit — "are there real callers of this symbol anywhere in the repo?"

Hermit's [`hermit.md`](../../defaults/.claude/commands/loom/hermit.md) already documents this exact
loop under "Dead Code":

```bash
rg "export function myFunction" --files-with-matches | while read file; do
  if ! rg "myFunction" --files-with-matches | grep -v "$file" > /dev/null; then
    echo "Unused: myFunction in $file"
  fi
done
```

and its "Be Specific and Evidence-Based" section makes the evidence standard explicit:

> "The `calculateTax()` function in src/lib/tax.ts is never called.
> Evidence: `rg 'calculateTax' --type ts` returns only the definition."

**That worked example contains the bug this whole evaluation is about.** `--type ts` scopes the
search to TypeScript. In this repo that would hide a reference from any of the 328 shell scripts,
231 Markdown files, or the `.yml`/`.json` config surface. `hermit-patterns.md` repeats the pattern
(`rg "SessionHandler" --type py --files-with-matches | grep -v test`). Language-scoped evidence is
Hermit's existing, documented, shipped hazard — and **every code-graph option in §4 would make it
worse, not better**, because a graph is language-scoped *by construction*.

Hermit's failure mode is asymmetric and the reverse of Judge's. A false *positive* caller (a grep
hit in a comment) means Hermit does not propose a deletion — safe, merely a missed opportunity. A
false *negative* means Hermit proposes deleting live code — unsafe, and it lands as a filed issue
someone else may act on. **Hermit should systematically prefer over-reporting references.** That is
precisely the opposite of what a precision-improving symbol graph optimizes for.

## 2. What Loom's codebase actually looks like

Measured at `090d15e0`:

```bash
git ls-files | sed 's/.*\.//' | sort | uniq -c | sort -rn | head
```

| Extension | Files | Lines |
|---|---:|---:|
| `.sh` | 328 | 143,430 |
| `.md` | 231 | 101,483 |
| `.rs` | 203 | 218,057 |
| `.ts` | 146 | 30,135 |
| `.tsx` | 41 | 3,625 |
| `.json` / `.yml` / `.toml` | 92 | — |

Two thirds of Loom's *files* are shell and Markdown. Role prompts are executable specification —
`.loom/roles/*.md` symlink into `defaults/.claude/commands/loom/*.md` and are the actual behavior of
every agent. Champion's own merge-risk table
([`champion-pr-merge.md`](../../defaults/.claude/commands/loom/champion-pr-merge.md) §"Blast radius")
enumerates Loom's highest-blast-radius surfaces by name, and they are almost entirely shell:
`merge-pr.sh`, `worktree.sh`, `loom-clean`, `.loom/hooks/guard-*.sh`, `spawn-claude.sh`,
`install-loom.sh`, `resync-installed.sh`.

The coupling is measurable:

```bash
rg -l 'merge-pr\.sh'   -g '!.loom/worktrees' . | sed 's/.*\.//' | sort | uniq -c | sort -rn
rg -l 'dispatch_sweep' -g '!.loom/worktrees' -g '!target' . | sed 's/.*\.//' | sort | uniq -c | sort -rn
```

| Referenced symbol | `.sh` | `.md` | `.rs` | `.ts` | other | Rust+TS share |
|---|---:|---:|---:|---:|---:|---:|
| `merge-pr.sh` (a script) | 38 | 14 | 9 | 0 | 0 | 15% |
| `dispatch_sweep` (**a Rust symbol**) | 5 | 25 | 17 | 3 | 2 | **38%** |

`dispatch_sweep` is the strong case for a call graph — a genuine Rust function — and a Rust+TS graph
still sees fewer than two in five of the files that reference it. The rest are role prompts telling
agents to call the MCP tool, docs describing it, and shell dispatch. **Loom's blast radius is
substantially a text-coupling problem, not a call-graph problem.** No amount of parser quality fixes
that, because there is no AST edge from a Markdown sentence to a Rust function.

## 3. There is no speed problem

```bash
$ time rg 'zzz_no_match_xyz' -g '!.loom/worktrees' -g '!node_modules' -g '!target' .
real    0m0.011s
$ time rg -n 'fn [a-z_]+' --type rust -o loom-daemon/src loom-api/src | wc -l   # 7,612 hits
real    0m0.007s
$ time rg -n 'export (function|const|class|interface|type) ' --type ts -o . -g '!node_modules'
real    0m0.007s
```

A **whole-repo scan that matches nothing** — the worst case, since every byte must be read — costs
**11 ms**. Tracked Rust+TS source totals 10.5 MB (`git ls-files -z '*.rs' '*.ts' '*.tsx' | xargs -0 du -cb`).

This kills the usual justification for an index outright. Indexes exist to amortize scan cost over
many queries; there is nothing here to amortize. Any index would pay a build cost measured in
seconds-to-minutes to accelerate a query that costs 11 ms. The *only* remaining argument for an
index is answer quality — which §2 shows a language-scoped index would make worse for these two
consumers, not better.

This is the same conclusion `docs/notes/prometheus-comparison.md` reached from a different direction
in 2026-07: "modern coding agents … are already effective at on-demand tree exploration with
ripgrep/read, which is the cheap, stateless substitute the KG would replace," with a persistent
graph rejected and a disposable per-sweep index filed as low priority, "only worth filing if
profiling shows exploration cost is material". **Profiling now shows it is not material.**

## 4. Survey: three concrete alternatives

Each is scored on what it would actually cost to wire into `judge.md` / `hermit.md`.

### Option A — `ast-grep` (structural pattern search, no index)

**What it is.** A Rust CLI wrapping tree-sitter for structural pattern matching:
`ast-grep run -p 'foo($$$ARGS)' -l rust`. Stateless — it parses on demand, stores nothing.
Multi-language, including Rust and TypeScript. This is the tool atomic-claude's own agents name as
their preferred search tool alongside grep.

**Cost to wire in.** Genuinely small: a `command -v ast-grep` probe (Loom already uses this pattern
in 101 scripts under `defaults/scripts/`, e.g. `check-runtime-capabilities.sh`, `validate-toolchain.sh`),
a thin wrapper script, and a paragraph in each role prompt. No index, therefore **no staleness
problem at all** — the strongest structural property of this option.

**What it buys.** Eliminates one class of grep false positive: a pattern like `$FUNC($$$)` will not
match the symbol appearing inside a string literal, a comment, or (importantly for this repo) prose.

**Why it still does not answer the question.** `ast-grep` matches *syntax*, not *bindings*. It
cannot tell you that this `run()` is `Sweep::run` and that one is `Daemon::run`; it does not resolve
imports, trait dispatch, generics, or re-exports. For Judge's "did this rename break a caller?" it
narrows candidates without resolving them. And its precision gain is aimed squarely at the direction
Hermit must not go (§1): fewer false-positive references means *more* confident deletion proposals
from a tool that is still blind to shell and Markdown references.

**Concrete deployment hazard, verified on this host.** ast-grep historically shipped an `sg` alias.
On a standard Linux host `sg` is shadow-utils' set-group-ID command:

```bash
$ sg --version
Usage: sg group [[-c] command]
$ dpkg -S /usr/bin/sg
login: /usr/bin/sg
```

A role prompt that says "run `sg …`" would silently invoke the wrong binary on every Debian/Ubuntu
fleet host — a plausible, hard-to-diagnose failure that produces confusing output rather than a
clean "not found". **If ast-grep is ever adopted, the canonical `ast-grep` name must be used and a
bare `sg` probe must be explicitly forbidden**, because `command -v sg` succeeds on this host today
and would pass a naive availability check.

**Verdict: the strongest of the three, and still not recommended now** — it addresses a precision
problem that has not been shown to cause bad verdicts (§8), on the two thirds of the reference
surface it cannot see.

### Option B — Language-server call hierarchy (`rust-analyzer` + `typescript-language-server`)

**What it is.** The only surveyed option that gives *real semantic resolution*:
`textDocument/prepareCallHierarchy` → `callHierarchy/incomingCalls` answers "who calls this?"
correctly, with imports, traits, and generics resolved. `rust-analyzer` is already on this host
(`/home/ubuntu/.cargo/bin/rust-analyzer`).

**Cost to wire in — disqualifying on three independent counts.**

1. **No CLI.** Neither server ships a "who calls X" command. Loom would have to write and maintain a
   JSON-RPC LSP client (spawn, `initialize`, wait for indexing to settle, `didOpen`, position-resolve
   the symbol, call hierarchy, shut down) — a real, stateful, per-language subsystem. That is the
   "own product" cost `docs/research/atomic-claude-evaluation.md` §2 already rejected, just wearing
   a different hat.
2. **Per-worktree build-class cost, against Judge's minutes budget.** `rust-analyzer` needs a
   resolved crate graph; on a fresh worktree that is a `cargo check`-class workload over 218k lines
   of Rust across two workspace crates. For scale, the main checkout's `target/` is **9.0 GB** and
   `.loom/worktrees/` is **28 GB across 119 worktrees** on this host today. Judge reviews a *branch
   worktree* (`judge.md` §"Worktree-Aware Code Access"), not main, so each review is a cold start.
   This directly violates the #3910 time budget.
3. **Silent-partial-answer risk.** An LSP that has not finished indexing returns *fewer* callers, not
   an error. For Hermit that is the exact unsafe direction: "0 callers" from a half-warm server is
   indistinguishable from "0 callers" from a correct one.

**Verdict: reject.** Best answers, worst cost, and its degradation mode is silently wrong rather
than absent — which the degradation contract in §5 forbids.

### Option C — A purpose-built, narrowly-scoped tree-sitter symbol map (Rust + TS only)

**What it is.** The shape the source issue floated and the shape
`docs/notes/prometheus-comparison.md` filed as "Spike candidate #2 — Disposable per-sweep symbol
index": a worktree-local, disposable map of definitions and references, built at sweep start,
dying with the worktree.

**Cost to wire in.** The highest ongoing cost of the three, and it is *maintenance*, not
construction. A first cut over `tree-sitter-rust` + `tree-sitter-typescript` is a weekend; keeping
it honest is not. Rust alone requires handling macros (`macro_rules!` bodies that construct call
sites), trait impls and dynamic dispatch, `#[derive]`-generated code, `cfg` gating, and re-exports.
A symbol map that silently mishandles any of these reports **zero callers for live code** — Hermit's
unsafe direction again.

**The index-sync problem this repo actually has** (the cost/maintenance check the issue asked for)
has no good answer at any of the three placements:

| Placement | Failure |
|---|---|
| **Repo-root, shared** | Immediately wrong for every worktree. Judge reviews `feature/issue-N`; an index built from `main` cannot see the symbols the PR added, and reports "no callers" for a function the PR itself just introduced. Actively misleading in exactly the case Judge is reviewing. |
| **Per-worktree, eager (at `worktree.sh` time)** | 119 live worktrees on this host. Every builder pays index-construction cost so that two support roles can occasionally consult it. `worktree.sh` is on the hot path of every single dispatch. |
| **Per-worktree, lazy (built inside the role)** | The whole construction cost lands inside Judge's minutes-scale budget, on a cold worktree, every review. |
| **Cached across worktrees, keyed by SHA** | Now it is a persistent store outside the forge — the ADR-0006 / ADR-0009 "state hidden from the forge" mistake `prometheus-comparison.md` §Q1 already rejected, plus cache invalidation across a `main` that merges several times a day. |

**Verdict: reject.** The source issue's own framing already contains the answer: "a stale or
per-worktree index is worse than no index if either role trusts it uncritically." Every placement is
either stale, expensive, or forge-invisible.

### Option D (brief) — `universal-ctags`

Mentioned only to close it out. Not installed on this host. A tags file indexes **definitions**, not
references, so it does not answer either role's question — Judge and Hermit both need reverse
lookup. Cheap and irrelevant.

## 5. Degradation contract (non-negotiable, applies to anything ever adopted here)

This is the issue's hard requirement and it is restated here as the binding rule for any future
work in this area. **Loom's roles must behave identically on a host that never set the tool up.**

1. **Absence is silent and normal.** Missing binary, missing helper script, missing index, malformed
   output, non-zero exit, timeout — all are the same state: *this signal is unavailable*. The role
   proceeds with its ordinary reading of the diff and its ordinary `rg`/`grep` searches.
2. **Never block, never fail the pass.** The helper must not be able to fail a role. Any invocation
   is wrapped so that a failure cannot propagate — no `set -e` propagation, no non-zero exit that a
   role's shell treats as fatal.
3. **Never surface unavailability to the forge.** No Judge review comment, no Hermit issue body, and
   no PR comment may mention that the tool or index was absent. A "code-intel unavailable" note is
   pure noise that teaches operators to install an optional thing, and it leaks host configuration
   into permanent forge history. Log it locally at most.
4. **Bounded time.** Every invocation carries an explicit `timeout` (Judge's #3910 rule). A timeout
   is treated as absence, per rule 1 — never as a reason to retry or wait.
5. **Evidence, never verdict.** Output is one input among several. A zero-reference result may
   **not**, on its own, justify a Hermit deletion proposal or a Judge approval; a nonzero result may
   not on its own block one. Both roles must corroborate — this is what stops a stale or
   partially-resolved answer from becoming a merged mistake.
6. **No persistence beyond the invocation.** Nothing written outside the worktree, nothing that
   survives the command, nothing committed, nothing gitignored-but-present that `loom-clean` or the
   worktree reaper would have to learn about.
7. **Opt-in and default-off.** Consistent with `buildGate`, `autonomous`, and `runtimes`: a repo
   with no configuration sees zero behavior change.

Rules 3 and 5 are the ones most likely to be eroded by a well-meaning later change, and they are the
two that matter most: rule 3 keeps the optional thing optional in practice, rule 5 is the only thing
standing between a wrong index and a bad merge.

## 6. What is actually worth building (candidate, not an approved build)

Not a graph. Not an index. A **stateless reference-evidence helper** — roughly 40–60 lines of shell
over `ripgrep`, with a `grep -r` fallback, no new binary dependency:

- **Input**: one or more symbol names. For Hermit, the symbol under investigation. For Judge, the
  symbols the diff removed/renamed (derivable from `git diff -U0 origin/main...HEAD` by extracting
  removed definition lines).
- **Search scope**: **every tracked file** — `git ls-files`, not a language filter. This is the
  single most important property and the direct fix for the `--type ts` hazard in `hermit.md`'s own
  worked example (§1).
- **Output**: references bucketed so the consuming role can weigh them, e.g. `definition`,
  `code-reference` (`.rs`/`.ts`/`.tsx`), `cross-language-reference` (`.sh`/`.yml`/`.json` — for Loom
  usually a live invocation, i.e. the *most* load-bearing bucket), `doc-reference` (`.md` — includes
  role prompts, which are executable specification here, so never dismissible as "just docs"), and
  `comment-or-string` (a best-effort heuristic, explicitly labelled as such).
- **Explicitly not**: resolution. It never claims "these are the callers". It says "here is every
  place this string appears, grouped so you can judge". That honesty is the entire design.

Why this is proportionate: it does nothing an agent could not do by hand with three `rg`
invocations, but it makes the *right* three invocations the default and makes whole-repo scope the
default instead of language scope. It is a checklist encoded as a script, which is what these two
roles actually need — not semantics.

**Also worth doing regardless, and cheaper**: fix the evidence standard in the role prompts
themselves. `hermit.md`'s "Be Specific and Evidence-Based" example and `hermit-patterns.md`'s
`--type py`/`--type ts` recipes teach language-scoped dead-code evidence. In a repo where 62% of the
files referencing a Rust symbol are neither Rust nor TypeScript (§2), that is a live hazard in shipped
guidance, and correcting it needs no tool at all. It should be filed as its own issue rather than
bundled into any tooling work.

## 7. Scope recommendation: broader than Rust+TS, and on a different axis

The issue asks for an explicit answer to "Rust+TS only, or broader". The answer is that the question
has the axis wrong, and the honest recommendation is both narrower and broader than it:

- **Narrower — structural/semantic parsing: none.** Not Rust+TS, not anything. No parser-backed
  index or graph is recommended for either role (§4).
- **Broader — reference evidence: all tracked files, no language filter.** Every `.sh`, `.md`,
  `.yml`, `.json`, `.toml`, and `.tsx`, not just the two compiled languages. Language-scoped
  evidence is the failure mode already present in shipped role prompts, and every graph option makes
  it structural rather than incidental.

If a future measurement (§8) overturns this, **`ast-grep` is the option to revisit first** — because
it is the only one that adds precision without adding an index, and therefore the only one that
cannot go stale. Under no circumstances should it *replace* the whole-repo text pass; it can only
annotate it.

## 8. Trigger for revisiting

This recommendation is explicitly evidence-gated rather than permanent. Reopen it when one of these
is observed, not before:

1. **Measured verdict harm.** Judge approvals that missed a caller the diff should have updated, or
   Hermit deletion proposals for live code, traceable to search imprecision — say, three or more
   instances found by an Auditor pass over merged PRs. Today this is speculated, not measured; §3
   shows the *cost* argument is already dead, so verdict quality is the only remaining case.
2. **Loom's language mix inverts.** If compiled code grows to dominate the reference surface — the
   Rust+TS share for a representative symbol going from 38% (§2) to a clear majority — the "a graph
   would be blind to most of it" argument weakens correspondingly.
3. **The scan stops being free.** If a whole-repo `rg` pass moves from ~11 ms into the
   hundreds of milliseconds *and* roles are running many of them per pass, the amortization argument
   for an index comes back. Re-measure with the §3 commands before assuming it.

If it is revisited, the §5 degradation contract and the §4 index-placement table are the constraints
any proposal must clear first — not afterthoughts.

## 9. What this evaluation deliberately did not do

- Did not vendor, copy, clone, install, or depend on any atomic-claude code. Its engine is described
  conceptually and by upstream file path only, via the prior evaluation in
  `docs/research/atomic-claude-evaluation.md` §2. No `atomic/` directory exists in this checkout.
- Did not install `ast-grep` or `universal-ctags`, and did not benchmark `rust-analyzer` call
  hierarchy latency. Option B is rejected on architecture (no CLI, per-worktree build-class cost,
  silent partial answers) rather than on a measured number; a benchmark would not change any of
  those three.
- Did not modify `judge.md`, `hermit.md`, `hermit-patterns.md`, any script, hook, or
  `.loom/config.json`. The role-prompt evidence-standard fix noted in §6 is called out as a
  separate, unfiled piece of work, deliberately not bundled here.
- Did not evaluate ideas 1, 3, 4, or 6 from the source evaluation — those have their own follow-up
  issues (#5847, #5849, #5850, #5851).
