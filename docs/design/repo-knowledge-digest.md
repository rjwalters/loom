# Repo knowledge digest: a generated, dirty-marked repo map for cold-start agents (#5847)

**Status:** design proposal. **No** runtime, role, hook, script, or config change ships with
this document — the only file in its PR is this one. Implementation is deferred to the slices
in §10, none of which are filed yet.
**Source issue:** [#5847](https://github.com/rjwalters/loom/issues/5847) — the **adopt** verdict
(idea 1) from the evaluation in `docs/research/atomic-claude-evaluation.md`
([#5844](https://github.com/rjwalters/loom/issues/5844)).
**Verified against:** `origin/main` @ `53984ad1`, 2026-08-09. Every path, line count, and
mechanism citation below was read at that commit — **re-verify before implementing**, several
are volatile by construction (§9).
**Upstream reference:** [damusix/atomic-claude](https://github.com/damusix/atomic-claude) (MIT),
read **only**. Nothing is vendored or ported; every mechanism below is reimplemented
Loom-shaped, and where this design deliberately deviates from upstream it says why.

---

## 1. Problem

Every Loom agent starts cold. A Builder dispatched into `.loom/worktrees/issue-N`, a Judge
reading a PR, a Curator enriching an issue — each begins with `CLAUDE.md` plus whatever it can
grep, and each re-derives the same handful of facts: *what builds this repo, what tests it,
where does functionality live, what are the surfaces*. That derivation is repeated per dispatch,
thousands of times, and thrown away every time.

The obvious place to put those facts is `CLAUDE.md` — and that is exactly the place they cannot
go. `scripts/check-claude-md-budget.sh` exists to stop it: *"CLAUDE.md is prepended to the
context of every session, every worker role, and every sweep child — a fixed per-dispatch tax
paid thousands of times a day… If you are over budget, relocate — do not raise the budget to fit
a reference dump."* As of `53984ad1`, `CLAUDE.md` is **320 lines against a 320-line budget** —
zero headroom. The budget is not an accident to be worked around; it is the correct response to
a **hand-maintained** file whose growth is unbounded and whose every individual addition is
defensible.

A **generated** digest is a structurally different artifact. Its content can be re-derived by a
machine, so it does not need a human to trim prose; it can be read **on demand** by the roles
that benefit rather than force-loaded into every dispatch; and it can carry its own staleness
metadata so a reader knows how much to trust it. That is the gap this design fills, and the only
one it fills.

### What transfers from atomic-claude, and what does not

Per `docs/research/atomic-claude-evaluation.md` §1, upstream's wiki pipeline is: a deterministic
scan captures raw facts, an inference pass synthesizes a compact router plus domain pages, a
`.dirty` marker file (cleared **only** on a fully clean refresh) plus a session-start nudge keep
staleness visible, and the compact router — never the raw scan — is auto-loaded into context.

Three of those four ideas transfer. The fourth does not, and it is the one that shapes this
entire design:

| Upstream mechanism | Transfers? | Loom form |
|---|---|---|
| Deterministic scan separated from inference | Yes | `repo-digest.sh render` (script) writes the auto region; the Guide writes the notes region (§4) |
| Compact digest, raw dump excluded from context | Yes | Hard line budget + explicit exclusions (§4) |
| Dirty marking that only clears on a clean full pass | Yes, **relocated** | A `render: partial` field **inside the committed artifact**, not a local `.dirty` file (§5) |
| Session-start hook nudges the session to refresh itself | **No** | There is no session to nudge. A scheduled role regenerates and ships a PR (§6) |

Upstream assumes one long-lived interactive session per repo that can notice it is stale and run
`/refresh-wiki` itself. Loom has no such session: every dispatch is an independent, ephemeral,
worktree-isolated process that may never come back, and several run concurrently on the same
repo. Both the storage decision (§3) and the ownership decision (§6) fall out of that difference.

---

## 2. What the digest is — and is not

**Is:** one committed Markdown file, ≤ ~200 lines, answering "what shape is this repo?" for an
agent that has never seen it — commands, directory→domain map, toolchain signals, surfaces, and
a small set of human-reviewed cross-cutting notes.

**Is not:**

- **Not a symbol graph / blast-radius index.** That is a separate proposal
  ([#5848](https://github.com/rjwalters/loom/issues/5848)) and explicitly out of scope here, per
  the source issue.
- **Not a raw scan dump.** Upstream keeps its multi-thousand-line `scan.md` out of the
  auto-loaded router for context reasons; this design goes further and does not produce a raw
  scan artifact at all — the generator's intermediate output is not committed.
- **Not project state.** Merged PRs, open issues, roadmap and priorities already live in
  `WORK_LOG.md` / `WORK_PLAN.md`, maintained by the same role on the same cadence. The digest
  describes the **code**, not the **queue**.
- **Not a replacement for `CLAUDE.md`.** `CLAUDE.md` holds durable *operating instructions*
  ("never use `gh pr merge`"). The digest holds *descriptive facts* ("the Rust workspace members
  are …"). Operating instructions must never migrate into a machine-overwritten file.
- **Not authoritative.** It is a cache of derived facts (§7). The repo is the source of truth.

---

## 3. Decision 1 — Where it lives: forge-committed, in the repo

**Decision:** `.loom/docs/generated/repo-digest.md`, tracked in git, updated only through a
normal reviewed PR.

### Why it must be forge-committed (the cold-start-agent argument)

This is the load-bearing decision, and it is forced by Loom's execution model rather than chosen
on taste. Consider each alternative against a concrete question: *a Builder is dispatched into a
fresh worktree on a host that has never run this repo before — can it read the digest?*

| Candidate home | Reachable from a cold worktree? | Fails because |
|---|---|---|
| **Committed file in the repo** ✅ | **Yes** — it is in the checkout, at a known path, at a known commit | — |
| Worktree-local scratch (`<worktree>/.loom-digest.md`) | No | `worktree.sh` creates each worktree from `origin/main`; anything not committed does not exist there. Every dispatch would regenerate it — which *is* the cold-start tax, relocated |
| Session-local / agent memory | No | Loom sessions are one-shot per dispatch. Nothing survives the process, and nothing is shared between the ~K concurrent agents on a host |
| Host-local cache (`~/.loom/digest/<repo>.md`) | Not portable | Dies on a second host, in CI, in the `loom-worker` container, and on any fresh clone. Worse, it is **invisible to review**: a wrong fact could sit in it indefinitely with no diff, no Judge, no history |
| Daemon state (SQLite / registry) | Only via the daemon | Couples a plain `Read` to a running daemon and an MCP round-trip. Roles dispatched by GitHub Actions cron have no daemon at all. Also unreviewable |
| Forge wiki / a gist | Extra fetch, unversioned w.r.t. the code | Cannot be pinned to the commit the agent is actually working at — the property §7 depends on |
| Inline in `CLAUDE.md` | Yes, but | Zero budget headroom (320/320), and it re-imposes the fixed per-dispatch tax on every role including those that never need it |

The committed-file answer also gives three properties nothing else does:

1. **It is versioned with the code it describes.** An agent at commit `X` reads the digest *as it
   was at `X`* — automatically, with no lookup. That is what makes concurrent-worktree divergence
   a non-problem rather than a race (§7).
2. **It is reviewable.** A regenerated digest arrives as a diff on a PR that a Judge reads.
   Upstream's answer to "don't let a machine silently rewrite your context" is per-item human
   confirmation in an interactive session; Loom's equivalent is the PR, and only a committed
   artifact can use it.
3. **It matches the architecture Loom already states.** `CLAUDE.md`: *"a forge (GitHub or Gitea)
   as the coordination layer."* The evaluation's §6 verdict rejected atomic-claude's Realm wiki
   for precisely this reason — *"a second, locally-compiled coordination-state store living
   entirely outside any forge… a `.dirty` marker on a local file nobody's dispatched agent is
   ever guaranteed to see."* Putting this digest in a local store would be that same mistake at
   smaller scale.

### Why that exact path

- **`.loom/docs/…`** puts it beside the other agent-facing reference docs an agent is already
  told to read, with the same path shape in a consumer repo as in this one.
- **`generated/` subdirectory, and it is load-bearing.** `.loom/docs/` is otherwise a mirror of
  `defaults/docs/` maintained by `resync-installed.sh` (`defaults/docs/ → .loom/docs/`,
  recursive). `defaults/docs/` contains **no subdirectories** as of `53984ad1`, so a `generated/`
  subdir is unambiguously "per-repo output, not install payload". Resync copies forward and does
  not prune destination-only files (`.loom/docs/survey-orca-2026-07-31.md` has no `defaults/`
  counterpart and survives), so the digest is safe — but the implementing slice should still list
  `docs/generated/repo-digest.md` in `.loom/resync-ignore` as belt-and-braces.
- **A flat `.loom/docs/repo-digest.md` would be wrong, and CI already says so.**
  `scripts/check-docs-defaults-parity.sh` fails any non-symlink `*.md` directly under
  `.loom/docs/` that has no `defaults/docs/` counterpart, because such a file "looks fine in-repo…
  but never ships" (#4841). A per-repo digest never shipping is *correct*, not a defect — so the
  flat path would need an `ORPHAN_ALLOWLIST` entry to suppress a check that is asking the right
  question. The subdirectory expresses "per-repo output" structurally instead, and sits outside
  the flat mirror that check governs (`find … -maxdepth 1` @ `53984ad1`). The implementing slice
  should still say so in a comment near the allowlist, so that a later depth change surfaces the
  decision rather than a surprise CI failure.
- **There must be no `defaults/docs/generated/repo-digest.md`.** `defaults/` is the *install
  payload*: a digest committed there would install **this repo's** digest into every consumer
  repo, describing a codebase the consumer does not have. The **generator** belongs in
  `defaults/scripts/` (so consumers get the capability); the **artifact** never does.

---

## 4. Decision 2 — What it contains

Two marker-delimited regions in one file, mirroring the discipline `WORK_PLAN.md` already uses
(`<!-- guide:plan-body:start -->` / `…:end -->`, everything between overwritten wholesale) and
upstream's scan-vs-infer split:

```markdown
<!-- loom:digest:meta
reflects_sha: 53984ad1
reflects_at: 2026-08-09T23:40:00Z
generator: repo-digest.sh/1
render: full            # full | partial  — "partial" is the dirty flag (§5)
partial_reason: ""      # non-empty iff render=partial
-->

> **Generated file.** Reflects `53984ad1` (2026-08-09). Advisory only — re-verify any
> fact before acting on it. Edit the generator, not this file.

<!-- loom:digest:auto:start -->
  … deterministic content, overwritten wholesale on every render …
<!-- loom:digest:auto:end -->

<!-- loom:digest:notes:start -->
  … short human/Guide-authored cross-cutting notes; the generator NEVER writes here …
<!-- loom:digest:notes:end -->
```

**Auto region** (deterministic; no LLM involved, so it is diff-stable and re-runnable by anyone):

| Section | Derived from | Why an agent needs it |
|---|---|---|
| Commands | `.loom/config.json` `buildGate.command` (`bash .loom/scripts/build-gate.sh` @ `53984ad1`), `package.json` scripts, workspace manifests, `.github/workflows/ci.yml` job steps | The single most re-derived fact in the repo: how to build/test/lint before committing |
| Directory → domain map | Top level + notable second level, one line each | Replaces "grep around until the right directory appears" |
| Toolchain signals | `rust-toolchain.toml`, `Cargo.toml` workspace members, `package.json` engines/packageManager | Prevents version-mismatch flailing |
| Surfaces & entry points | Binaries, MCP servers, `.github/workflows/*`, script categories under `.loom/scripts/` (84 entries @ `53984ad1` — an index by category, **not** a file listing) | Orients a Judge on blast radius without a symbol graph |
| Stamped counts | Anything volatile, each rendered with its own `as of <sha>` | Makes staleness self-evident at the point of use |

**Notes region** (inference; small, human-reviewable, never machine-overwritten): 5–10 lines of
cross-cutting conventions a scan cannot see — e.g. "role prompts under `.loom/roles/*.md` are
symlinks into `defaults/.claude/commands/loom/`; edit the canonical file." Kept in a separate
region precisely because `WORK_PLAN.md`'s own guidance warns that hand-written annotation left
*inside* a generated region is silently wiped on the next tick.

**Size budget.** Hard cap (proposed **200 lines**), enforced by `repo-digest.sh check` in the
same spirit as `check-claude-md-budget.sh`. The generator drops lowest-priority sections rather
than overflow, and **truncation sets `render: partial`** — the digest reports its own
incompleteness instead of silently shipping a half-map.

---

## 5. Decision 3 — What triggers regeneration (the dirty check)

`repo-digest.sh stale` is a deterministic predicate: exit `0` = fresh, exit `10` = stale, plus a
one-line reason on stdout. It fires on the **OR** of four conditions:

1. **Structural touch** — `git diff --name-only <reflects_sha>..HEAD` intersects a tracked glob
   set: `Cargo.toml`, `*/Cargo.toml`, `package.json`, `rust-toolchain.toml`,
   `.github/workflows/*`, `.loom/config.json`, added/removed top-level directories, added/removed
   `.loom/scripts/*`. These are the inputs the auto region is derived from; if none changed, a
   re-render is almost always byte-identical.
2. **Volume** — total changed lines since `reflects_sha` exceeds a threshold (proposed **2000**).
   Catches broad drift that never touches a manifest, and is the direct analogue of upstream's
   incremental-vs-full refresh decision.
3. **Age** — `reflects_at` older than **14 days**. The notes region's inference can rot without
   any structural signal; age is the only trigger that catches it.
4. **Dirty flag** — `render: partial`. This is upstream's `.dirty` semantic, relocated: **it stays
   stale until a clean full render clears it.** A truncated, errored, or interrupted render leaves
   the flag set, so the next tick retries instead of accepting a degraded digest as current.

**Why the flag lives inside the artifact.** Upstream can use a local `wiki/.dirty` file because
one session on one machine both sets and reads it. Loom has no shared local filesystem across
the fleet — a `.dirty` file written on host A is invisible to host B, to CI, to the container,
and to the next fresh clone; a worktree-local one is invisible even to the next dispatch on the
*same* host. The only medium every cold-start reader is guaranteed to see is **the committed file
itself**, so the dirty bit must be a field in it. This also makes the flag reviewable: a PR that
lands `render: partial` shows exactly that in its diff.

**Structural detail:** conditions 1 and 2 both need `<reflects_sha>` to be an ancestor of `HEAD`.
If it is not (force-push, a very old branch, a shallow clone), `stale` must return **stale with
reason `unknown-base`** rather than erroring — the check is advisory infrastructure and must never
be the thing that fails.

---

## 6. Decision 4 — Who owns it: the Guide's Document Maintenance phase

**Decision:** the **Guide** role, as a new step inside its existing Document Maintenance phase,
riding the **same bundled docs PR** as `WORK_LOG.md` / `WORK_PLAN.md` / `README.md`.

Guide is not merely the closest fit — it is the only role that already has every piece of
machinery this needs, all of which exists at `53984ad1`:

- `.loom/scripts/docs-worktree.sh` — a managed worktree with a `.loom-managed` sentinel, because
  the role runner starts scheduled roles in the **main checkout**, where `guard-worktree-paths.sh`
  and `guard-destructive-generic.sh` deny writes (`Edit`, and Bash `>`/`tee`/`sed -i`/`cp`/`mv`
  alike). Any design where some other agent writes the digest in place is structurally
  impossible, not merely discouraged.
- `.loom/scripts/docs-guide-lock.sh` — serializes ticks on one host.
- The Step-5 **cross-host recheck** — re-runs the open-docs-PR search immediately before
  `push`+`create` (deliberately with uncached `gh`) to narrow the multi-host TOCTOU window.
- A **no-op guard** — `git diff --cached --quiet` → no commit, no PR. A byte-identical re-render
  therefore produces no churn, which is what makes a frequent staleness check affordable.
- The marker-region overwrite discipline the digest's auto region copies.

Rejected alternatives:

| Owner | Why not |
|---|---|
| Each Builder, at dispatch | K concurrent worktrees ⇒ K conflicting digest diffs, in a file no Builder's issue is about. Turns a shared file into a rebase-conflict magnet and pollutes every PR's diff |
| The daemon, writing `main` directly | Guards deny it; it is a silent unreviewed write to `main` (the thing the source issue rules out); and it needs a running daemon, which cron-dispatched roles do not have |
| A new dedicated role | Would contend with Guide for the same docs-PR slot and duplicate the worktree/lock/recheck machinery. Two writers, one file, zero benefit |
| A CI workflow on push to `main` | Requires CI write access through the branch ruleset, and produces an unreviewed commit. Also cannot do the notes region |
| Curator / Champion / Judge | Forge-read-only by construction; none has a write worktree |

**Cadence:** Guide's existing tick. Step order: after the `WORK_PLAN.md` step, run
`repo-digest.sh stale`; if fresh, skip (the common case); if stale, `repo-digest.sh render
--out "$DOCS_WT/.loom/docs/generated/repo-digest.md"`, review the notes region if the domain map
changed, `git add` it alongside the other three files, and let the existing Step 5 do the rest.
The digest never gets its own PR.

---

## 7. Decision 5 — Staleness mid-fleet: advisory, never blocking

> **A stale digest degrades guidance quality. It must never block, gate, fail, or delay
> anything.** This is a hard contract, not a default.

### What "multiple concurrent worktrees" actually looks like

With K worktrees branched from different points of `main`, **K different digest versions are live
at once**, and each agent reads the one at its own branch point. There is nothing to reconcile:

- **Reads are snapshot-consistent by construction.** The digest is a committed file, so an agent
  at commit `X` reads the digest as of `X` — the version that best matches the tree it is actually
  looking at. A shared mutable store would have handed it a version describing a tree it does not
  have.
- **There is exactly one writer.** Only Guide's docs branch ever edits the path. A Builder PR
  cannot conflict with a digest refresh because Builder PRs never touch that path — the digest can
  therefore never be the reason a PR needs a rebase.
- **Two Guide ticks racing is already handled, and losing is free.** The lock plus the cross-host
  recheck usually prevent a second PR; if one lands anyway, the loser's local commit is discarded
  rather than merged. Nothing is lost, because a render is a pure function of `HEAD` — the next
  tick reproduces it.
- **A merge conflict in the auto region is resolved by re-rendering**, never by hand-merging two
  machine outputs.

### The failure mode that actually matters

Not divergence — **misplaced trust**. An agent reads "the check command is `X`" after `X` was
renamed. Three things bound that cost:

1. **The digest states its own age.** The banner and `reflects_sha` are visible in the first lines
   of the file, so a reader can always see how far behind it is.
2. **Consuming roles are told to re-derive before acting.** This is not a new rule — `builder.md`
   § "Re-Verify Date-Stamped Facts Before Acting" already requires exactly this for
   Curator-stamped facts, and is strictest where it matters most (irreversible actions: version
   bumps, tag pushes, publishes). The digest's per-fact `as of <sha>` stamps make it the same
   class of input.
3. **Wrong facts fail loudly and cheaply.** A stale command errors on the first invocation and the
   agent falls back to reading config — one wasted tool call. Compare with the status quo, where
   *every* agent pays the derivation cost on *every* dispatch. The expected value stays positive
   even at a fairly high staleness rate.

### Explicitly out of bounds for any implementer

The digest **must not** appear in any of these, now or later:

- `buildGate` (`.loom/config.json` → `bash .loom/scripts/build-gate.sh`) — a stale digest must
  never fail a Builder's gate or release a claim.
- Sweep pre-flight, or any dispatch-time precondition.
- A guard hook (`PreToolUse` deny/ask).
- A **required** CI check.
- A Judge rejection reason, or a Champion merge hold.

Anything that turns "the map is a bit old" into "work stops" converts a pure optimization into a
fleet-wide outage vector — and would do so at the worst possible moment, since the digest is most
likely to be stale exactly when the repo is changing fastest.

**A non-required CI *warning*** (annotation only, never `exit 1`) is acceptable and probably
useful, purely as a nudge that Guide's next tick has work to do.

### Why this differs from `AGENTS.md` — the correct asymmetry

`scripts/check-agents-md-sync.sh` is a **hard** CI check on a generated, checked-in artifact, and
that is right: `defaults/.loom/AGENTS.md` is a pure deterministic function of marker ranges in one
committed source file, so "stale" is unambiguous, always the author's fault, and fixable by one
command in the same PR.

The digest is not that. Its input is the entire repo state; "stale" is a matter of degree; the
person who would be blocked (a Builder touching `Cargo.toml`) is not the owner of the fix (Guide);
and its notes region is inference, which cannot be byte-compared at all. A hard check here would
fail PRs for a condition their author did not cause and cannot correctly resolve. Same artifact
posture, deliberately opposite enforcement — and this paragraph exists so a future contributor
sees the asymmetry as intentional rather than as an oversight to "fix".

---

## 8. Consumption, and the `CLAUDE.md` pointer problem

**Do not auto-load the digest.** Upstream force-loads its router into every session via an
`@docs/wiki/index.md` reference in `CLAUDE.md`. Loom must not: that reintroduces the exact fixed
per-dispatch tax the budget check exists to bound, and it would make every role pay for a file
most of them will not use on any given dispatch. On-demand `Read` is also what keeps §7's
"degrades, never blocks" property trivially true — an agent that never reads it is unaffected by
its staleness.

**The pointer is not free.** `CLAUDE.md` is **320 lines against a 320-line budget** at
`53984ad1`: adding a single pointer line fails CI. Options:

| Option | Assessment |
|---|---|
| (a) Net-zero edit in `CLAUDE.md` — trade a line to add the pointer | Viable, but spends scarce shared budget on a file most dispatches will not read |
| (b) **Point from consuming role prompts** (`.loom/roles/builder.md`, `judge.md`, …) — not budget-checked | **Recommended for the first slice.** Only the roles that benefit pay the context cost, and it can be rolled out one role at a time and measured |
| (c) Raise the budget | **Rejected** — the budget script's own header forbids exactly this ("Do NOT raise the budget to fit a reference dump") |

Recommendation: (b) now; revisit (a) only once the digest has demonstrated value across several
roles. Whichever is chosen, the pointer text must carry the advisory-only framing — a pointer that
reads like an authoritative source of truth invites precisely the misplaced trust §7 is built to
contain.

---

## 9. Risks and failure modes

| Risk | Severity | Mitigation |
|---|---|---|
| Agent trusts a stale fact and acts on it | Medium | Visible `reflects_sha` banner + per-fact stamps + the existing re-verify rule (§7) |
| Digest grows into a second `CLAUDE.md` | Medium | Hard 200-line cap enforced by `check`; overflow truncates and sets `render: partial` |
| Someone hand-edits the auto region | Low | Marker discipline + the banner; the next render overwrites it, and the wholesale overwrite makes the loss visible in the diff |
| Digest churn adds PR noise | Low | No-op guard: a byte-identical render produces no commit; it rides an existing PR and never opens its own |
| Non-determinism in the generator (timestamps, `find` ordering) causes spurious diffs | **Medium** | Renders must be byte-stable for a given `HEAD`: sort every listing, and derive `reflects_at` from the commit's own date, **not** wall clock |
| Two hosts' Guides race | Low | Existing lock + cross-host recheck; a lost race discards a reproducible render |
| Someone later adds a blocking check | **High if it happens** | §7's out-of-bounds list, stated as a contract in the doc *and* required in the implementing slice's PR body |
| Digest leaks into `defaults/` and installs into consumer repos | Medium | §3: generator ships in `defaults/scripts/`, artifact never does; CI can assert `defaults/docs/generated/` does not exist |

---

## 10. Implementation slices (proposed; none filed yet)

1. **Generator + first artifact.** `defaults/scripts/repo-digest.sh` with `render`, `check`,
   `stale`; the first committed `.loom/docs/generated/repo-digest.md`; `.loom/resync-ignore`
   entry. No role wiring. Independently verifiable: two consecutive renders at the same `HEAD` are
   byte-identical; `stale` exits `10` after a manifest change and `0` otherwise; `check` fails a
   201-line file.
2. **Guide wiring.** Document Maintenance Step 4b: `stale` → `render` into `$DOCS_WT` → stage
   alongside the existing three files. No new PR path, no new lock.
3. **Consumers.** Pointer + advisory-only contract text in the role prompts that benefit (Builder
   and Judge first), per §8 option (b).
4. **Notes region (optional, last).** Guide authors the inference region when the domain map
   changes. Deferred deliberately: its quality is unmeasurable until 1–3 are in use.

Ordering rationale: slice 1 is useful standalone (an operator can run it by hand); slice 3 without
1–2 points at nothing; slice 4 is the only part whose value is speculative.

---

## 11. Open questions for the operator

1. **Line budget** — 200 proposed. Too tight for a repo with this many surfaces?
2. **Age trigger** — 14 days proposed; it is the only trigger that fires with no code change, so
   it is also the only one that can produce a PR nobody asked for.
3. **`CLAUDE.md` pointer** — accept recommendation (b) (role prompts only), or spend a
   `CLAUDE.md` line now?
4. **Consumer repos** — ship the generator in `defaults/scripts/` from slice 1, or dogfood here
   first and propagate later?
5. **Non-required CI warning** — wanted, or is even an annotation more noise than nudge?

---

## 12. What this design deliberately does not do

- Does not vendor, copy, or port any atomic-claude code or prose. Every mechanism above is
  described conceptually and reimplemented for Loom's model; upstream's MIT license imposes no
  attribution obligation on a conceptual adaptation, and none of the slices in §10 propose copying
  literal text.
- Does not propose any symbol graph, tree-sitter parsing, or code-intelligence index
  ([#5848](https://github.com/rjwalters/loom/issues/5848)).
- Does not change `CLAUDE.md`, any role prompt, `.loom/config.json`, any hook, or any script —
  those are slices 1–3.
- Does not measure the cold-start cost it aims to reduce. The premise (agents re-derive repo shape
  every dispatch) is taken from `CLAUDE.md`'s own budget rationale and the evaluation's §1, not
  from instrumentation. If slice 3 is to be judged on value rather than plausibility, that
  measurement is the missing piece.
