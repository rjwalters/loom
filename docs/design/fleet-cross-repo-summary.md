# Fleet-level cross-repo summary for multi-repo Loom hosts — decision (#5851)

**Status:** design decision. **Recommendation: build nothing new.** The problem this issue
describes — an operator on a host running several Loom-managed repos has no single place to see
status across them — is not a gap. It is already solved, shipping, and **verifiably running right
now on this very host**, by the existing multi-repo daemon plus `loom-daemon status`/`serve`. No
runtime, role, hook, script, or config change ships with this document.
**Source issue:** [#5851](https://github.com/rjwalters/loom/issues/5851) — the **adapt** verdict
(idea 6, the "Realm") from the evaluation in `docs/research/atomic-claude-evaluation.md`
([#5844](https://github.com/rjwalters/loom/issues/5844)).
**Verified against:** `origin/main` @ `85549250`, 2026-08-09 for every doc/code citation below —
**plus this host's own live, running `loom-daemon` process** at the same date, queried directly
(`loom-daemon workspace list`, `loom-daemon status --json`) rather than inferred from
documentation. Re-verify code citations before acting; the live-host numbers are a point-in-time
snapshot by construction and will already have drifted.
**Upstream reference:** [damusix/atomic-claude](https://github.com/damusix/atomic-claude) (MIT),
read **only**. Nothing is vendored or ported.

---

## Answers, up front

| # | Question the issue asked | Answer |
|---|---|---|
| 1 | Which problem does this solve — dashboard rollup, or cross-repo agent context? | **Dashboard rollup only.** §2 scopes cross-repo agent context out, explicitly and for longer than this issue. |
| 2 | Design it on `loom-daemon serve` / `observability`, not a new local state file | **No new design needed at all.** §3 shows the rollup already exists, already reuses exactly those surfaces, and is already running — verified live on this host, not just read in docs. |
| 3 | Priority vs. the wiki-digest proposal (#5847) | **Lower, and by a wide margin.** §6: #5847 has real unshipped work (an adopted design with zero slices filed); this proposal's finding is "nothing to build." |

---

## 1. Problem, restated precisely

The source evaluation (`docs/research/atomic-claude-evaluation.md` §6) and the issue body both
frame this as two different, easily-conflated problems:

- **(A) Dashboard rollup** — "I run several Loom-managed repos on one host and have no single view
  of what's happening across them" (active sweeps, review-queue backlog, operator holds, per-repo
  health).
- **(B) Cross-repo agent context** — "an agent working in repo A needs facts about repo B" (e.g. a
  shared library's conventions, a sibling repo's recent breaking change).

The issue's own §"Proposed design questions" already flags these as different in kind and asks
this document to pick one. It also correctly identifies (B) as depending on the wiki-digest
proposal (#5847, this same research pass's **adopt** verdict) landing first — you cannot summarize
facts about repo B across repos before you can summarize facts about repo B *at all*.

## 2. Decision: (A) is in scope, (B) is explicitly out of scope

This document addresses **(A) only**. **(B) is out of scope** — not deferred to a later section
of this same document, but excluded from this proposal's design entirely, for three independent
reasons:

1. **No artifact to aggregate yet.** #5847 (the per-repo knowledge digest) is an **adopted, but
   unimplemented** design — its own doc states "Implementation is deferred to the slices in §10,
   none of which are filed yet." A cross-repo layer over a per-repo artifact that does not exist
   is not a design problem yet; it is a dependency-ordering problem, and the dependency has not
   cleared.
2. **No architectural seam for it.** Every Loom dispatch — Builder, Judge, Curator, Doctor — is
   scoped to exactly one repo for its whole lifetime: one worktree, one issue/PR, one forge
   remote (`CLAUDE.md` §"Git Worktree Workflow"). Nothing in the current sweep model has a role
   mid-task that needs to reach into a *different* repo's state. Designing a cross-repo read path
   for a consumer that does not exist yet would be speculative infrastructure.
3. **No observed need.** Unlike (A) — which this document shows is already a live, measurable
   capability people are already using (§3) — (B) has zero concrete instances of a Loom role
   actually stalling on "I don't know something about a sibling repo." The issue itself only
   poses it as a hypothetical ("a much harder... problem").

If a real cross-repo-agent-context need ever surfaces, it should be scoped and designed as its own
issue once #5847 has shipped and produced a real per-repo artifact to point at — not folded into
this decision preemptively (see §7 trigger).

## 3. Finding: the dashboard-rollup problem is already solved, and is running today

This is the load-bearing finding of this document, and it is not a documentation-reading
conclusion — it was checked against the actually-running daemon on the host this issue was filed
from.

### 3a. The multi-repo daemon is not hypothetical — it is the default fleet-worker shape

`.loom/docs/daemon-reference.md` §"Delegated daemon administration" states the architecture
plainly: *"the daemon is host-global (one binary, one socket, one `~/.loom/workspaces.json`)."*
This is not a rarely-used mode — it is literally step 7 of the documented fleet-worker bootstrap
plan (`fleet add_worker.rs`, §"workspace-register" — *"`loom-daemon workspace add` each repo at
`--priority`"*), the standard path by which every fleet worker host gets provisioned.

**Live check on this host, 2026-08-09:**

```
$ loom-daemon workspace list
Managed workspaces (24):
Registry file: /home/ubuntu/.loom/workspaces.json
...
  PRIO  WORKSPACE
  ------------------------------------------------------------
     3  /tmp/mc-debug/repo
     5  /home/ubuntu/GitHub/sky130-asic-puzzle
    10  /home/ubuntu/GitHub/loom
    12  /home/ubuntu/GitHub/repo
    15  /home/ubuntu/GitHub/anvil
    20  /home/ubuntu/GitHub/klayout-tools
   ... (24 total)
```

This is precisely the scenario the issue's "Why" section cites as motivation — *"this very host,
for instance"* runs several Loom-managed repos side by side — except the premise that follows
("with no shared view across them") does not hold: one daemon process already governs, and already
reports on, all 24.

### 3b. `loom-daemon status --json` already returns a per-repo fleet summary

`DaemonStatusReport.per_repo: Vec<RepoStatus>` (`loom-daemon/src/types.rs`) is populated by
enumerating `WorkspaceRegistry::effective_roots()` — every registered repo, not just the daemon's
own primary workspace (`.loom/docs/daemon-reference.md` §"Per-repo status breakdown", #3930 phase
d). **Live output from this same host, same date** (`loom-daemon status --json`, `.per_repo`,
truncated to relevant fields for one entry):

```json
{
  "root": "/home/ubuntu/GitHub/loom",
  "priority": 10,
  "in_flight_count": 1,
  "health_gate_halted": false,
  "role_runner_roles": ["champion", "curator", "judge", "doctor", "guide"],
  "stash": { "total_count": 12, "quarantine_count": 8, "oldest_age_secs": 935241 }
}
```

Repeated once per registered repo, in one JSON response, from one IPC round-trip, with **zero
forge calls** (the dispatch-side view is read entirely from the daemon's own in-memory
registries — see the module doc on `pipeline_snapshot.rs` distinguishing this from the
forge-side `--pipeline` view in §3c). This is already: repo name (`root`), active sweep count
(`in_flight_count`), and a health signal (`health_gate_halted`) — three of the four fields the
issue's own "minimum viable cross-repo view" asks for, computed live, per repo, today.

### 3c. `loom-daemon status --pipeline` (and `serve`'s `/api/pipeline`) already adds forge-side, per-repo queue/throughput counts

The forge-side pass — the one metric bucket §3b's dispatch-side view deliberately excludes to
stay fast and network-free — is `pipeline_snapshot.rs`'s `RepoPipelineSnapshot`, one instance per
managed repo, fanned out in parallel across all of them
(`.loom/docs/daemon-reference.md` §"Forge queue metrics", `collect_pipeline_snapshots`):

| Field | Meaning |
|---|---|
| `queued` | Open, dispatchable `loom:issue` rows (park-labeled rows excluded) |
| `building` | Open `loom:building` (claimed, in progress) |
| `review_requested` | Open PRs awaiting Judge |
| `changes_requested` / `changes_requested_unclaimed` | Doctor's queue, and the no-owner subset (#5272) |
| `approved` | Judge-approved, awaiting Champion merge |
| `merged_24h` | PRs merged in the last 24h — a throughput signal |

This is exposed identically over `GET /api/pipeline` on `loom-daemon serve`
(`.loom/docs/daemon-reference.md` §"Fleet dashboard", "Forge-side queue counts per managed repo…
fronted by a 20s in-process cache"), and the dashboard's single HTML page already renders it —
the doc's own description of what the page shows lists **"per-repo pipeline queue counts"**
alongside the dispatch-side per-repo panel from §3b, on the same page, for every registered repo.

### 3d. For genuinely separate daemon processes: `--peers` gives the identical rollup with no server-side code either

Not every host consolidates every repo under one daemon's workspace registry (a repo can opt out
of shared administration via `daemon.delegatedTo`, or an operator may simply run isolated
per-repo daemon+config pairs). For that shape, `loom-daemon serve --peers
http://host2:7420,http://host3:7420` already exists for exactly this case
(`.loom/docs/daemon-reference.md` §"Fleet dashboard": *"multihost fleet view"*) — the served
`/api/peers` route hands the browser the peer list, and **the browser fetches each peer's own
`/api/status`/`/api/pipeline` directly**; the serving daemon never proxies or aggregates them
server-side. Nothing about that client-side fan-out design requires the peers to be on different
hosts — pointing `--peers` at `http://127.0.0.1:7421,http://127.0.0.1:7422` (separate `serve`
instances for separate repos, same host, different ports) produces the same one-page rollup with
**zero new code**, today.

### 3e. Mapping to the issue's proposed minimum viable view

| Proposed field | Already available via | Gap |
|---|---|---|
| Repo name | `RepoStatus.root` / `RepoPipelineSnapshot.root` | none |
| Active sweep count | `RepoStatus.in_flight_count` | none |
| Open `loom:operator` count | — | **yes — see §4** |
| Last-merge time | `RepoPipelineSnapshot.merged_24h` (a 24h throughput count, not a bare timestamp) | partial — see §4 |

## 4. The one real gap: no `loom:operator` count, and no bare "last merge" timestamp

`RepoPipelineSnapshot` counts `loom:issue`, `loom:building`, and the three review-side labels, but
**not** `loom:operator` — the merge-risk hold `CLAUDE.md` describes as *"the first-class 'a human
is needed' state ... wired at Champion's merge-risk hold only so far"*. An operator scanning a
26-repo fleet for "which repo is silently waiting on me" cannot get that count from any existing
route today; they would have to open each repo's PR list.

`merged_24h` is the existing throughput proxy for "last merge," and it is arguably the *better*
fleet signal, not a lesser one: a bare "time since last merge" timestamp is noisy for a low-traffic
repo (a healthy quiet repo and a stalled one both show "N days ago"), while a 24h rate at least
distinguishes "actively shipping" from "idle." Nonetheless it does not answer literally what the
issue asked for.

**Recommendation on this gap: do not file it now.** It is a small, additive
`PipelineMetrics`/`RepoPipelineSnapshot` field (one more `gh` count, mirroring the shape of the
seven that already exist) if and when an operator actually wants it — see the trigger in §7. Filing
it speculatively, with no operator behind it, would be exactly the kind of unrequested surface area
this decision is otherwise arguing against adding.

## 5. Why this satisfies the source issue's own constraint

The evaluation this issue was filed from was explicit that a literal port of atomic-claude's
Realm — a locally-compiled `wiki/` directory, a second coordination-state store living outside any
forge — is the wrong shape for Loom, because "GitHub/Gitea labels are the coordination state"
(`CLAUDE.md`). This decision does not merely avoid repeating that mistake; it does not add *any*
storage, generator, cache, or artifact, local or forge-committed. Every fact rendered by §3 is
computed live, per request, from the daemon's own in-memory registries and forge queries that were
already being made for the single-repo `status`/`serve` surfaces before this issue existed — there
is no new persistence layer of any kind to evaluate for the wiki-store failure mode.

## 6. Priority vs. the wiki-digest proposal (#5847): lower, and by a wide margin

The issue's acceptance criteria call for stating this explicitly. It is not a close call:

- **#5847 has real, adopted, unshipped work.** Its own design doc (`docs/design/repo-knowledge-digest.md`)
  proposes a generator script, a Guide-owned regeneration cadence, a staleness contract, and four
  implementation slices — none filed yet, but all concretely scoped and worth doing.
- **This proposal's finding is "there is nothing to build."** The dashboard-rollup framing (§2's
  in-scope half) is already shipped and already running; the one identified gap (§4) is explicitly
  recommended *against* filing until a real operator asks for it.
- **The cross-repo-agent-context framing (§2's out-of-scope half) structurally depends on #5847.**
  Even if it is ever pursued, it cannot start before #5847's per-repo digest exists to be
  aggregated across repos.

Both halves of this proposal therefore rank behind #5847 in any prioritization: one half is already
done, and the other half's prerequisite has not shipped.

## 7. Trigger for revisiting (the only conditions under which this decision changes)

- **An operator explicitly asks** for a per-repo `loom:operator` (or similar hold-label) count on
  `status --pipeline` / the dashboard → a small, additive `PipelineMetrics` field, no architecture
  change, no new design doc needed — just implement it the way `changes_requested_unclaimed` (#5272)
  was added to the same struct.
- **#5847 ships**, and some role demonstrably stalls on "I don't know something about a different
  repo" mid-dispatch → scope and file cross-repo agent context as its own new issue at that point,
  informed by whatever the shipped digest actually looks like — not by guessing now.

Absent either trigger, this proposal's action item is **none**.

## 8. What this decision deliberately does not do

- Does not propose, design, or scope any new local file, database, cache, or `wiki/`-style
  artifact, forge-backed or otherwise.
- Does not implement the `loom:operator`-count gap identified in §4 — noted as an ungated future
  candidate only, explicitly not filed as an issue by this document.
- Does not change `loom-daemon serve`, the `observability` config block, any role prompt,
  `CLAUDE.md`, or any label.
- Does not design cross-repo agent context ((B) in §1–§2) beyond scoping it out — a genuinely
  different, harder problem with no current consumer, gated on #5847 shipping first (§7).
- Does not vendor, copy, or port any atomic-claude code or prose; every mechanism cited above is
  Loom's own, pre-existing implementation, read and verified against a live daemon, not
  reconstructed from upstream's design.

## References

- `.loom/docs/daemon-reference.md` §"Delegated daemon administration", §"Per-repo status
  breakdown + per-repo main-health gate", §"Fleet dashboard"
- `loom-daemon/src/types.rs` (`RepoStatus`), `loom-daemon/src/pipeline_snapshot.rs`
  (`RepoPipelineSnapshot`, `PipelineMetrics`)
- `loom-daemon/src/fleet/add_worker.rs` (fleet-worker bootstrap step 7, `workspace-register`)
- `docs/design/repo-knowledge-digest.md` (#5847, the adopted-but-unimplemented sibling proposal)
- `docs/research/atomic-claude-evaluation.md` §6 (source verdict for this issue)
- Live evidence: `loom-daemon workspace list` and `loom-daemon status --json` run against this
  host's own running daemon, 2026-08-09 (24 registered workspaces at the time of writing)
