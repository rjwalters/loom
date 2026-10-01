# ADR-0023: Combined-PR consolidation contract (eligibility, ownership, failure recovery)

## Status

Accepted (design record for #9687, first-stage increment of the #9063 epic; candidate preparation #9688 and landing #9689 implement separate portions of this contract)

## Context

The #9063 operator decision sequenced the epic: automatic ordering first (#9686, landed as the merge-sequencing pass), then a reviewed contract for automatically prepared combined PRs, then the preparation and landing implementations. This ADR is that contract. It is deliberately written against the primitives that now exist on `main`/review rather than inventing parallel ones:

- **#9378** (merged): the durable `loom:sequenced` gate — a label the merge path refuses beside `loom:pr`, with the condition carried in a trusted `<!-- loom:sequence … -->` marker and evaluated fail-closed by `merge-pr sequence-eval`.
- **#9686** (PR #9707): the ordering pass — overlap components, oldest-first chain orders, deterministic `seq-` plan ids, head-pinned `source=pass` markers, soft-hold expiry, and base-repair deferral.
- **Observed evidence** from #9686's read-only replay (recorded in PR #9707): a real 3-PR overlap chain identified on live inventory; ordering holds behave as specified across repeated passes.

The contract's obligations, restated from #9687: eligibility inputs and bounds, state transitions, ownership, cancellation, retry limits, failure/recovery rules with **no unspecified destructive step**, worked lifecycle examples, measurement definitions, and preparation/landing boundaries with stable ids for `2AMLogic/loom-ui#590` to consume. Git commit landing is atomic; multiple forge PR/issue updates are not one atomic transaction — reconciliation is resumable and idempotent rather than "atomic closure".

## Decision

### 1. Reservations are sequencing holds — the #9378 primitive, not a new one

A consolidated attempt **reserves** each source PR by applying the existing `loom:sequenced` label with a marker whose predecessor is **the candidate PR itself**:

```text
<!-- loom:sequence after=<candidate PR> pred_head=<candidate head> follower_head=<source head> plan=<attempt-id> source=pass -->
```

Consequences, all inherited rather than re-implemented:

- "A source PR cannot land concurrently with the candidate that includes it" is enforced by the existing verdict-contradiction gate on every merge path — zero new shell, zero new gate logic.
- When the candidate lands, the merge-sequencing pass mechanically releases the reservation (predecessor merged at recorded head ⇒ `CLEAR`).
- An **abandoned** attempt's reservations self-release via the soft-hold expiry bound (72 h of predecessor quiet on an approved source) — the contract's "bounded retry/staleness" story is the expiry bound, not a new timer.
- A push to the **candidate** branch voids every reservation pinned to the old head (`PredecessorMoved` ⇒ `VoidAndReplan`, the ordinary ordering-pass behavior). This is expected mid-attempt — Doctor fixing the candidate after a Judge rejection (Example 3) is a normal part of its lifecycle, not a failure — so the next `consolidate-prepare`/`consolidate-reconcile` tick re-pins every live source's reservation to the new candidate head (§5's "Re-pin source reservations" step). Between the push and that re-pin, a voided source is an ordinary, unreserved PR; the accepted window this opens is named in Consequences/Negative.
- A push to a **source** PR's branch while its candidate is open voids only that source's own reservation (`FollowerMoved` ⇒ `VoidAndReplan`) and is **not** re-pinned: construction fixed that source's pinned head before Judge ever reviewed the candidate, so a new source head was never part of what was reviewed. v1 does not silently swap it in — it hard-aborts the whole attempt, exactly as Example 5 hard-aborts on a head push during preparation (§3).

The attempt id **is** the plan id, in a distinct namespace: `cons-` + 8 hex of SHA-256 over the sorted `number:head` component pins (same derivation as the `seq-` ordering ids, so one id format, two prefixes). `source=pass` marks these as soft, machine-managed holds; a human replacing one with a source-less marker converts it to a hard hold, which is the documented manual-override path.

### 2. Eligibility — inputs and bounds (all checked fresh, immediately before any mutation)

A group is consolidation-eligible when **every** item holds at preparation time:

| # | Input | Rule |
|---|---|---|
| E1 | Repository / base | All components target the same repository and the same base branch (the repo default). |
| E2 | Open, non-draft, pinnable | Every component PR is open, not a draft, with a full-length head SHA to pin. |
| E3 | No holds | No component carries `loom:blocked`, `loom:operator`, `loom:operator-only`, `loom:operator-decision`, `loom:changes-requested`, or `loom:ci-failure`. Operator holds (E3 rows 2–4) are absolute exclusions preserved from the original operator ruling. |
| E4 | No workflow edits | No component's changed files touch `.github/workflows/**`. |
| E5 | No active claim | No component carries `loom:reviewing`/`loom:treating`/`loom:building`. |
| E6 | Not already reserved | No component carries a live `loom:sequence` marker whose `plan=` names a different open attempt. |
| E7 | Order-compatible | No component is sequenced behind an unlanded predecessor outside the group (in-group predecessors are fine — the group lands in its order). |
| E8 | Related scope + rationale | The components share changed files (overlap evidence, never proof) AND the caller records a compatibility rationale (`--reason`), which is copied into the candidate PR body. Path disjointness alone never qualifies; file overlap alone never auto-qualifies. |
| E9 | Bounded review size | ≤ `LOOM_CONSOLIDATE_MAX_COMPONENTS` components (default 4) and ≤ `LOOM_CONSOLIDATE_MAX_DIFF_LINES` total added+deleted lines (default 800) across the group. A combined review larger than the bound defeats the purpose: one review that nobody completes. |
| E10 | Clean construction | Merging the pinned heads in landing order succeeds with no conflict. A conflict is a **hard abort** — v1 does no evict/retry/bisect search. |

### 3. Attempt lifecycle and state transitions

```text
planned ──prepare──> candidate_open ──canonical merge──> landed ──reconcile──> reconciled
   │                     │                                                        (terminal)
   │                     ├──abort──> aborted (terminal)
   └──conflict/cancel──> aborted (terminal)
                         
candidate_open ──(72 h quiet)──> expired ≡ aborted  [via soft-hold expiry, per-source]
```

- `planned → preparing`: eligibility E1–E9 verified fresh; attempt id derived.
- `preparing → candidate_open`: candidate branch pushed, candidate PR created, reservations applied. Each step is independently resumable (see §5).
- `candidate_open → candidate_open` (self-loop, no state change): a push to the candidate branch — Doctor addressing Judge feedback, a `main`-merge to clear a conflict — voids every source reservation pinned to the old head; the next `consolidate-prepare`/`consolidate-reconcile` tick re-pins each live source to the new head (§1, §5). This is the ordinary mid-review lifecycle (Example 3), not a failure.
- `candidate_open → landed`: the **canonical merge path only** (`merge-pr.sh` on the candidate PR). The preparation machinery never merges; the landing machinery never merges. All existing preconditions (fresh `loom:pr`, green required checks, hold guards, #9161 composites) apply to the candidate exactly as to any PR.
- `landed → reconciled`: the reconciliation loop walks the mapping and finishes bookkeeping (§6). Safe to re-run from any interruption; a restart after landing finishes bookkeeping rather than merging again.
- `candidate_open → aborted`: via explicit `consolidate-abort` (failed candidate CI, operator decision, **or a push to any source PR's branch while the candidate is open**, §1) — closes the candidate PR and releases **only this attempt's** reservations. A green component never grants approval or CI success to the combination, a failed candidate CI run preserves every original PR untouched, and a source-push abort leaves every component — including the one that moved — untouched-open.
- `expired`: an attempt whose reservations quietly expired (72 h) with the candidate still open is treated as abandoned; the next reconciliation touch closes the stale candidate PR and releases what remains, or a human aborts it.

### 4. Identity and ownership

- **Attempt id**: `cons-<8 hex>` as in §1. Deterministic from components+heads: two workers preparing the same group derive the **same** id and converge on one candidate instead of racing two.
- **Ownership**: the attempt owns exactly its reservations — markers whose `plan=` matches its id. No attempt ever releases another attempt's reservation. Release itself is not reconciliation's job: the merge-sequencing pass already releases a reservation mechanically the instant its predecessor (the candidate) merges at the recorded head (§1, `CLEAR`) — the same zero-new-mechanism path every ordinary `source=pass` hold uses. Reconciliation's own "release reservations" step (§5, §6.4) is therefore an idempotent, observe-only check, not an actor: by the time reconciliation reaches it, the pass has normally already cleared the hold. The accepted consequence is the small window this opens between landing and reconciliation's own closure of the component PR — see Consequences/Negative.
- **The ledger is the candidate PR body** (trusted markers, owner/repo-qualified): component number, pinned head, scope rationale, compatibility note, attempt id, candidate head at prepare time. It must be reconstructible from the candidate PR alone — no local-only state, so any daemon or human can resume.

### 5. Destructive-step table (complete — no unspecified destructive step)

| Step | Precondition | Retry story | Interrupted run leaves behind |
|---|---|---|---|
| Push candidate branch `loom/consolidated/<attempt>` | E1–E10 pass; clean construction | Idempotent: branch already at the expected head ⇒ adopt | Orphan branch, no PR ⇒ next prepare adopts it, or `consolidate-abort` deletes it |
| Create candidate PR | Branch pushed | Adopt-first: an open PR with that head branch is adopted, never duplicated | Orphan branch without PR (same as above) |
| Apply reservation to source | Candidate PR exists | Exact-marker idempotency (identical marker ⇒ no comment) | Source PR mergeable and un-reserved ⇒ it may merge normally (safe: candidate not yet ready); next run re-applies |
| Re-pin source reservations to new candidate head | Attempt still `candidate_open`; live candidate head differs from the attempt's recorded `pred_head` | Idempotent: marker already pinned to current head ⇒ no-op | Voided sources stay unreserved — ordinary, mergeable PRs — until the next tick re-pins them; bounded by that tick's cadence, the same cadence the merge-sequencing pass already runs on (no new timer) |
| Post candidate mapping | Candidate PR exists | Body is written once at creation; later edits are additive comments | — (body-only, no effect) |
| Post per-component status (landing) | Candidate merged; inclusion verified | Idempotent: status comment presence checked | Missing status ⇒ reconcile again posts it |
| Remove source reservation (landing) | Candidate merged at the recorded head — evaluated mechanically by the merge-sequencing pass, independent of reconciliation's own progress | Idempotent: label absent ⇒ no-op | Reconciliation does not force removal; a reservation still present here only means that pass has not ticked yet — the next pass tick clears it, and reconciliation does not wait on or re-check it |
| Close component PR | Status posted; verified included | Idempotent: already-closed ⇒ no-op | PR stays open ⇒ reconcile again closes it |
| Close linked issue | Component closed; `Closes/Fixes/Resolves` reference extracted via the existing refs analysis; underlying issue acceptance-criteria gate respected | Idempotent: already-closed ⇒ no-op | Issue stays open ⇒ reconcile again; **unincluded or partly satisfied work stays open** |
| Delete candidate branch | Attempt `reconciled` (or `aborted`) | Idempotent: already-deleted ⇒ no-op | Branch remains ⇒ cleanup next run; never force-cleans stacked children (#9372 guards respected) |

Every idempotency check reads live state before acting (label set, PR state, comment presence); none trusts a cached "already done".

### 6. Landing reconciliation order (each step resumable)

1. **Verify the landing**: candidate PR is merged; record the merge SHA. Never re-merge.
2. **Verify inclusion per component**: each pinned head is an ancestor of the candidate branch head recorded in the mapping (construction merged pinned heads, so ancestry is the containment proof; the recorded head — not a live re-read — is what the candidate's CI tested).
3. **Post per-component status** on each source PR: `merged-into <candidate> (#N) at <merge SHA>` with owner/repo-qualified bidirectional links. Status vocabulary, pinned: `merged-into` (diff contained in the verified landing), `superseded-by` (deliberately excluded/replaced), or untouched-open. GitHub marking a PR "closed" is never reported as "merged".
4. **Confirm reservations are released.** This step does not itself release anything: the merge-sequencing pass already released each reservation mechanically when the candidate landed (§1). It only observes the expected absence; a reservation still present here means that pass has not ticked yet, which reconciliation does not wait on or force.
5. **Close component PRs** with the exact combined merge SHA in the closure comment.
6. **Close linked issues** whose component PR carried closing references — extracted with the existing refs analysis, subject to the existing issue-close-gate semantics; the closure comment links the candidate PR and merge SHA. Issues without closing references get the status comment and stay open.
7. **Delete the candidate branch** (protected/stacked children respected, #9372).

### 7. Measurement definitions (consumed via existing events; `2AMLogic/loom-ui#590`)

- **Avoided base-only repairs**: count of repair-routing events (`loom:merge-conflict` flags / Doctor `rebase` rework markers) on component PRs while reserved, versus the same PRs' pre-attempt rate. Attributed causes only; unknown causes stay unknown (reported as such, never extrapolated).
- **Review/CI attempts**: combined candidate's Judge verdicts + CI runs versus the counterfactual sum of per-component runs — reported as observed counts, with the counterfactual labeled as an estimate.
- **Queue age**: candidate PR age at merge versus the components' ages; per-event, no aggregation beyond mean/median, sample size stated.
- **Useful review findings**: Judge findings on candidates that would NOT have existed per-component (e.g. integration conflicts between components) — counted explicitly, because this is the risk side of the trade.

No performance-improvement claim without these numbers; the pilot reports what happened, including nothing.

### 8. Preparation / landing implementation boundaries

- **Preparation** (#9688): `loom-daemon merge-pr consolidate-prepare --repo O/R --pr N… --reason "…"` and `consolidate-abort --pr <candidate>`. New Rust under `loom-daemon/src/merge_pr/consolidate.rs` (+ tests, incl. the lifecycle fixtures for §5's scenarios); CLI wiring under `cli/`. Reads plan/eligibility state; writes branch, candidate PR, reservations, comments.
- **Landing** (#9689): the merge itself is `merge-pr.sh` on the candidate — the canonical path, unchanged. Reconciliation is `merge-pr consolidate-reconcile --pr <candidate>` over the same module, plus (later) a reconciliation-tick hook. Stable ids/events for #590: attempt ids (`cons-…`), the event names in §3's transitions, and the marker/comment surfaces already defined here.
- The two stages share this module's mapping parser and helpers; neither reaches into the other's write surface.

### 9. Composability review (against the contracts this must not conflict with)

- **#9378**: reservations *are* sequencing holds (§1) — one label, one gate, one evaluator; no new merge-path machinery, no bypass.
- **#9416**: no proven-equivalence carryover is used or implied; the combined diff always gets its own fresh Judge verdict and its own CI runs.
- **#9372**: candidate branches live in the Loom-managed `loom/consolidated/` namespace; cleanup runs last and never force-cleans a stacked child; components with stacked children fail E7 (their children are out-of-group predecessors).
- **#9161**: the candidate merges through the canonical path, so every existing merge precondition composes unchanged — consolidation grants no exemption, ever.

## Worked lifecycle examples (each maps to a fixture in `merge_pr/consolidate` tests)

1. **Overlapping files**: #10 and #12 both edit `daemon/src/run.rs`, neither held. Prepare ⇒ attempt `cons-a1b2c3d4`, candidate PR #99 (body maps both, pins heads, carries the recorded rationale), reservations `after=#99` on both. Judge approves #99's combined diff; CI green; #99 merges via `merge-pr.sh`, which mechanically releases both reservations; reconcile verifies ancestry, posts statuses, confirms the releases, closes #10/#12 with the merge SHA, closes their `Closes:` issues, deletes `loom/consolidated/cons-a1b2c3d4`.
2. **Semantic dependency**: #20 grows a fake-set, #21 dedups it — order matters (#21 after #20). E7 allows in-group ordering; construction merges in the recorded order; the ADR's position: consolidation preserves the ordering pass's landing order inside the candidate; a semantic dependency *across* an eligibility boundary (predecessor outside the group) blocks E7 until it lands.
3. **Substantive rejection**: during `candidate_open`, Judge rejects the combined diff (`changes-requested`). The candidate is an ordinary PR: Doctor fixes the candidate branch, or the operator aborts. Doctor's push voids every source's reservation and the next tick re-pins them to the fixed candidate's new head (§1, §5) — nothing else propagates to the sources; their statuses stay open and untouched.
4. **Held / workflow PRs**: #30 carries `loom:operator-only`, #31 edits `.github/workflows/ci.yml`. Both fail E3/E4 at eligibility, before any mutation; the verb reports each rejection reason and does nothing.
5. **Head push during preparation**: #40's head moves after pins are read but before the reservation lands. The reservation marker pins the NEW head only if re-read; the pinned-head check fails ⇒ construction/validation aborts the attempt (hard abort, no silent re-pin), sources untouched.
6. **Duplicate workers**: two daemons run `consolidate-prepare` for the same group concurrently. Same attempt id ⇒ both converge: one pushes/creates, the other adopts the existing branch/PR and skips re-posting markers (exact-marker idempotency). No duplicate candidate.
7. **Failed candidate CI**: #99's combined CI is red. The candidate is not mergeable (existing guards). The operator/Champion runs `consolidate-abort`: candidate PR closed, reservations released, sources preserved and actionable exactly as before the attempt. No auto-evict/retry in v1; a bounded retry is a fresh attempt with a new id.
8. **Interruption during component closure**: reconcile crashes after closing #10 but before #12. Restart: candidate merged (step 1 verify idempotent), #10's status exists (skip), #12's status posted, reservations already released by the merge-sequencing pass (confirmed, no-op), #12 closed, issues closed, branch deleted. Every step's precondition is re-read; nothing double-applies.

## Consequences

### Positive

- Fleet-scale repair churn from overlapping landing becomes schedulable without any new merge-path mechanism.
- Abandoned attempts self-heal (expiry) and interrupted ones self-resume (idempotent reconciliation); no orphaned gating state.
- The candidate is judged on its own merits — the approval/CI surface never widens.

### Negative

- A combined review bundles risk: a rejection blocks all components until repaired or aborted (accepted; v1 has no eviction, and the abort path keeps sources actionable).
- Reservation release vs. reconciliation is a small race window (a released source merging before reconciliation closes it); accepted — its diff is already on main, and the closure is a no-op then.
- A candidate push opens an analogous window before the next tick re-pins the voided sources (§1, §5): a source could merge independently while unreserved. Accepted for the same reason — its pinned head was already fixed into the candidate at construction time, so the candidate's own content is unaffected, and §5's "Close component PR" step finds it already closed and no-ops.
- Two more verbs and a module in the already-large merge surface; accepted — the alternative (prompt-side convention) is exactly the unenforced-bytes failure mode this contract exists to avoid.

## Alternatives Considered

- **Per-component eviction/retry on candidate failure** — rejected for v1: an automatic evict/retry/bisect search multiplies CI cost and can silently drop a component; the explicit abort keeps sources actionable. Revisit only with measurement data demanding it.
- **A new `loom:reserved` blocking label** — rejected: duplicates #9378's gate with a second label to teach every merge path about; the sequencing-hold form reuses the existing refusal, release, and expiry machinery.
- **Atomic multi-PR closure** — impossible on the forge (each update is a separate fallible write); the resumable reconciliation is the contract's answer, not an approximation of a lost ideal.
- **Constructing the candidate from patches/cherry-picks** — rejected: merge commits of pinned heads make inclusion verifiable by ancestry (§6.2), which patch application does not preserve.

## References

- Related GitHub Issues: #9063 (epic), #9687 (this record), #9688 (preparation), #9689 (landing/reconciliation), #9378 (the gate), #9686 (ordering pass), #9372 (stacked children), #9416 (equivalence carryover — explicitly not used), #9161 (merge preconditions), 2AMLogic/loom-ui#590 (measurement consumer)
- Related ADRs: ADR-0022 (merge commit default — the landing shape), ADR-0018 (Rust owns behavior, shell reaches it)
- Fixture home: `loom-daemon/src/merge_pr/consolidate/tests.rs` (#9688/#9689)
