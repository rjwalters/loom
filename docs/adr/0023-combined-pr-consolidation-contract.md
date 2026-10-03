# ADR-0023: Combined-PR consolidation contract (eligibility, ownership, failure recovery)

## Status

Accepted (design record for #9687, first-stage increment of the #9063 epic; candidate preparation #9688 and landing #9689 implement separate portions of this contract). Revised 2026-10-01 per the operator ruling on #9733: any push aborts the attempt, and the ordering pass alone releases reservations on landing.

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
- When the candidate lands, the merge-sequencing (ordering) pass mechanically releases the reservation (predecessor merged at recorded head ⇒ `CLEAR`). **The ordering pass is the one owner of release on landing**; consolidation code never removes a reservation because the candidate landed (§4).
- An **abandoned** attempt's reservations self-release via the soft-hold expiry bound (72 h of predecessor quiet on an approved source) — the contract's "bounded retry/staleness" story is the expiry bound, not a new timer. A lost reservation ends the attempt (§3).
- **Any push aborts the attempt** (operator ruling, 2026-10-01). A reservation pins both heads, so a push to the candidate (`PredecessorMoved`) or to a source (`FollowerMoved`) makes the ordering pass void that hold. The contract does not re-pin, repair or carry the attempt forward: a push to the candidate PR or to **any** source PR while the candidate is open aborts the whole attempt (§3 "Push abort"), its remaining holds are released, and the next ordering pass re-plans the sources from their current heads.

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

**In-group ordering markers (E6/E7).** A source may already carry an in-group `seq-` ordering marker from the ordering pass. There is one marker per follower, so applying the `cons-` reservation replaces it: the newest trusted marker wins. The construction order comes from the **recorded plan**, which is read before any reservation is applied and is copied into the ledger (§4). It does not come from the live marker, so once a reservation has replaced the `seq-` marker, nothing reads that marker for ordering again. If the attempt aborts, the ordering pass re-derives a fresh `seq-` plan for the sources from their current heads.

### 3. Attempt lifecycle and state transitions

```text
planned ──prepare──> candidate_open ──canonical merge──> landed ──reconcile──> reconciled
   │                     │                                                        (terminal)
   │                     ├──abort──> aborted (terminal)
   │                     ├──push to candidate or any source──> aborted (terminal)
   │                     └──reservation lost (void / 72 h expiry / removed)──> aborted (terminal)
   └──conflict/cancel/push during preparation──> aborted (terminal)
```

- `planned → preparing`: eligibility E1–E9 verified fresh; attempt id derived.
- `preparing → candidate_open`: candidate branch pushed, candidate PR created, reservations applied, then every source head re-read against its pin. A moved head aborts, with no silent re-pin. Each step is independently resumable (see §5).
- `candidate_open → landed`: the **canonical merge path only** (`merge-pr.sh` on the candidate PR). The preparation machinery never merges; the landing machinery never merges. All existing preconditions (fresh `loom:pr`, green required checks, hold guards, #9161 composites) apply to the candidate exactly as to any PR, plus one candidate-only precondition: the pins are intact (see "Push abort" below).
- `landed → reconciled`: the reconciliation loop walks the mapping and finishes bookkeeping (§6). Safe to re-run from any interruption; a restart after landing finishes bookkeeping rather than merging again. Landing is the point of no return. A push observed after the candidate merged is **not** an abort; §6 step 2 says what happens to that source.
- `candidate_open → aborted`: via explicit `consolidate-abort` (failed candidate CI, operator decision), or implicitly through "Push abort" below. The abort closes the candidate PR, deletes its branch and releases **only this attempt's** still-live reservations. A green component never grants approval or CI success to the combination, and a failed candidate CI run preserves every original PR untouched.

**Push abort (operator ruling, 2026-10-01).** An open attempt is live only while all three of these hold: (i) the candidate's live head equals the candidate head recorded in the ledger; (ii) every source's live head equals its pin; (iii) every reservation is still live (label present, no `released`/`replanned` tombstone after it). If any of them fails before landing, the attempt is **aborted**. That covers a Doctor fix, a merge of `main`, or a CI fix pushed to the candidate; an author's push to a source; a reservation voided, expired or removed by hand. Aborted means aborted: no re-pin, no repair of the candidate in place, no carrying the attempt forward. The abort steps are the `consolidate-abort` steps. Holds the ordering pass already voided read as released and are skipped (no-op). Once the sources are released, the next ordering pass re-plans them from their current heads. A later consolidation of the same PRs is a **fresh attempt**: new heads give a new `cons-` id.

- **Who detects it:** the reconciliation touch of an open, unmerged candidate (the same verb and tick hook as landing, §8) runs the three-part check and aborts when it fails. As a second line, the candidate's merge precondition re-runs the check immediately before the canonical merge, so a source push can never land as a stale candidate between two reconciliation ticks.
- **The window:** a push makes the ordering pass void the moved holds on its next tick. That can happen before the drift check closes the candidate. In that window the sources are unreserved while the candidate is still open. This is safe because the candidate cannot merge (the pre-merge pin check refuses it, and a candidate push also invalidates its head-pinned Judge verdict). A source that merges in the window only confirms the abort. Candidate PRs (`loom/consolidated/` head refs) are excluded from ordering-pass eligibility, so the pass never chains a candidate behind its own sources.
- **Expiry** is one case of a lost reservation. After 72 h of candidate quiet, the pass expires a soft reservation on an approved source, and the next reconciliation touch aborts the attempt.

### 4. Identity and ownership

- **Attempt id**: `cons-<8 hex>` as in §1. Deterministic from components+heads: two workers preparing the same group derive the **same** id and converge on one candidate instead of racing two.
- **Ownership**: the attempt owns exactly its reservations — markers whose `plan=` matches its id. No attempt ever releases another attempt's reservation.
- **Who releases a reservation.** Each event has exactly one releaser:

  | Event | Releaser | Mechanism |
  |---|---|---|
  | Candidate lands | **The ordering pass, only.** | `CLEAR` (predecessor merged at the recorded head) ⇒ `Release`. Reconciliation never removes the label on landing; its §6 step 4 is observe-only. |
  | Push to candidate or source | The ordering pass, for each moved hold | `PredecessorMoved`/`FollowerMoved` ⇒ `VoidAndReplan`. The resulting abort then releases the siblings that are still live. |
  | Abort (explicit or push abort) | The abort step | Releases this attempt's **still-live** reservations. A hold that is already voided or released is skipped. |
  | 72 h quiet | The ordering pass | Soft-hold `Expire`; the attempt is then aborted (§3). |

  **What landing release implies.** After the candidate lands, the pass releases the sources before reconciliation closes them. Until they are closed they are ordinary open PRs. They still overlap one another, so the pass may re-plan them **among themselves** with `seq-` holds. That is harmless: reconciliation closes a verified source whatever its label state, and a `seq-` hold on a closed PR is inert, because the pass only evaluates open PRs. A released source may also *merge* in that window (see Consequences). Reconciliation never waits for the release. In a repository where the ordering pass is not running Phase 1 (see the follow-up), reconciliation still completes, and the stale label on a closed PR has no effect.
- **The ledger is the candidate PR body** (trusted markers, owner/repo-qualified): component number, pinned head, scope rationale, compatibility note, attempt id, recorded construction order, candidate head at prepare time. It must be reconstructible from the candidate PR alone — no local-only state, so any daemon or human can resume. The recorded candidate head is the reference for the push-abort check (§3).

### 5. Destructive-step table (complete — no unspecified destructive step)

| Step | Precondition | Retry story | Interrupted run leaves behind |
|---|---|---|---|
| Push candidate branch `loom/consolidated/<attempt>` | E1–E10 pass; clean construction | Idempotent: branch already at the expected head ⇒ adopt | Orphan branch, no PR ⇒ next prepare adopts it, or `consolidate-abort` deletes it |
| Create candidate PR | Branch pushed | Adopt-first: an open PR with that head branch is adopted, never duplicated | Orphan branch without PR (same as above) |
| Apply reservation to source | Candidate PR exists | Exact-marker idempotency (identical marker ⇒ no comment) | Source PR mergeable and un-reserved ⇒ it may merge normally (safe: candidate not yet ready); next run re-applies |
| Post candidate mapping | Candidate PR exists | Body is written once at creation; later edits are additive comments | — (body-only, no effect) |
| Push abort: close candidate, release live reservations, delete branch | Candidate open and unmerged; the §3 three-part check fails (candidate head ≠ recorded head, a source head ≠ its pin, or a reservation lost) | Idempotent: candidate already closed ⇒ no-op; label absent or hold already tombstoned ⇒ skip. The check is re-read every touch, so a lost run is re-detected next tick | Candidate open with some holds released ⇒ the pre-merge pin check still refuses it; the next touch finishes the abort |
| Post per-component status (landing) | Candidate merged; inclusion verified | Idempotent: status comment presence checked | Missing status ⇒ reconcile again posts it |
| Release source reservation (landing) | — **not a reconciliation step**: the ordering pass releases on `CLEAR` (§4) | Pass's own idempotency (label absent ⇒ not a holder) | Label still on ⇒ the pass releases it on a later tick; reconciliation does not wait |
| Close component PR | Status posted; verified included; source's live head still equals its pin | Idempotent: already-closed ⇒ no-op | PR stays open ⇒ reconcile again closes it |
| Close linked issue | Component closed; `Closes/Fixes/Resolves` reference extracted via the existing refs analysis; underlying issue acceptance-criteria gate respected | Idempotent: already-closed ⇒ no-op | Issue stays open ⇒ reconcile again; **unincluded or partly satisfied work stays open** |
| Delete candidate branch | Attempt `reconciled` (or `aborted`) | Idempotent: already-deleted ⇒ no-op | Branch remains ⇒ cleanup next run; never force-cleans stacked children (#9372 guards respected) |

Every idempotency check reads live state before acting (label set, PR state, comment presence); none trusts a cached "already done".

### 6. Landing reconciliation order (each step resumable)

1. **Verify the landing**: candidate PR is merged; record the merge SHA. Never re-merge.
2. **Verify inclusion per component**: each pinned head is an ancestor of the candidate branch head recorded in the mapping (construction merged pinned heads, so ancestry is the containment proof; the recorded head — not a live re-read — is what the candidate's CI tested). Also compare each source's **live** head with its pin. A source pushed after the candidate merged is past the abort point, since the landing already happened. It is **not** closed: it gets the status `untouched-open` with a note naming the pinned head that landed, its linked issues stay open, and it continues as an ordinary PR.
3. **Post per-component status** on each source PR: `merged-into <candidate> (#N) at <merge SHA>` with owner/repo-qualified bidirectional links. Status vocabulary, pinned: `merged-into` (diff contained in the verified landing), `superseded-by` (deliberately excluded/replaced), or untouched-open. GitHub marking a PR "closed" is never reported as "merged".
4. **Observe reservation release (no write).** The ordering pass releases reservations on `CLEAR` (§4). Reconciliation does not remove the label, does not post a release, and does not wait for the release. It only records in its report whether each label is still on.
5. **Close component PRs** with the exact combined merge SHA in the closure comment, whatever the reservation's state (step 4).
6. **Close linked issues** whose component PR carried closing references — extracted with the existing refs analysis, subject to the existing issue-close-gate semantics; the closure comment links the candidate PR and merge SHA. Issues without closing references get the status comment and stay open.
7. **Delete the candidate branch** (protected/stacked children respected, #9372).

### 7. Measurement definitions (consumed via existing events; `2AMLogic/loom-ui#590`)

- **Avoided base-only repairs**: count of repair-routing events (`loom:merge-conflict` flags / Doctor `rebase` rework markers) on component PRs while reserved, versus the same PRs' pre-attempt rate. Attributed causes only; unknown causes stay unknown (reported as such, never extrapolated).
- **Review/CI attempts**: combined candidate's Judge verdicts + CI runs versus the counterfactual sum of per-component runs — reported as observed counts, with the counterfactual labeled as an estimate.
- **Queue age**: candidate PR age at merge versus the components' ages; per-event, no aggregation beyond mean/median, sample size stated.
- **Useful review findings**: Judge findings on candidates that would NOT have existed per-component (e.g. integration conflicts between components) — counted explicitly, because this is the risk side of the trade.
- **Aborts by cause**: `candidate-push`, `source-push`, `reservation-lost`, `ci-failure`, `operator`, `construction-conflict`. These are counted per attempt from the abort comment. Push-abort makes consolidation fragile when authors are active, so this rate is the data on which to revisit that ruling.

No performance-improvement claim without these numbers; the pilot reports what happened, including nothing.

### 8. Preparation / landing implementation boundaries

- **Preparation** (#9688): `loom-daemon merge-pr consolidate-prepare --repo O/R --pr N… --reason "…"` and `consolidate-abort --pr <candidate>`. New Rust under `loom-daemon/src/merge_pr/consolidate.rs` (+ tests, incl. the lifecycle fixtures for §5's scenarios); CLI wiring under `cli/`. Reads plan/eligibility state; writes branch, candidate PR, reservations, comments.
- **Landing** (#9689): the merge itself is `merge-pr.sh` on the candidate — the canonical path, unchanged. Reconciliation is `merge-pr consolidate-reconcile --pr <candidate>` over the same module, plus (later) a reconciliation-tick hook. On an open candidate, the same verb runs the push-abort check (§3) and the abort; on a merged one, it runs §6. Landing also owns the candidate-only pre-merge pin check. Stable ids/events for #590: attempt ids (`cons-…`), the event names in §3's transitions, and the marker/comment surfaces already defined here.
- The two stages share this module's mapping parser and helpers; neither reaches into the other's write surface.

### 9. Composability review (against the contracts this must not conflict with)

- **#9378**: reservations *are* sequencing holds (§1) — one label, one gate, one evaluator; no new label, no new marker source, no bypass. The push-abort rule needs no change to the evaluator: it treats the existing `PredecessorMoved`/`FollowerMoved` voids as the end of the attempt.
- **#9686**: the ordering pass is the sole landing releaser (§4). Its Phase 1 must run whenever there are holders, independent of the open-PR planning trigger, and its eligibility excludes `loom/consolidated/` candidates (§3). Both are follow-ups. Since #10060 the ordering pass's own edges are direct-overlap only (a follower waits for its nearest earlier member that shares a changed file or that it is stacked on, never for a PR linked only through a third PR's files), and a soft `seq-` hold on an approved follower is released early when its predecessor is a stalled head (quiet past `LOOM_MERGE_SEQUENCE_STALL_HOURS`, default 12, on a human hold or without a verdict) and escalated once on that head. That stall release deliberately skips `cons-` reservations, whose only staleness bound stays the 72 h expiry (§1), and never touches a hard (source-less) hold.
- **#9416**: no proven-equivalence carryover is used or implied; the combined diff always gets its own fresh Judge verdict and its own CI runs.
- **#9372**: candidate branches live in the Loom-managed `loom/consolidated/` namespace; cleanup runs last and never force-cleans a stacked child; components with stacked children fail E7 (their children are out-of-group predecessors).
- **#9161**: the candidate merges through the canonical path, so every existing merge precondition composes unchanged — consolidation grants no exemption, ever. The one addition is a candidate-only precondition (pins intact, §3). It is a refusal composed into the existing precondition set, not a new gate or a bypass, and it can only make a merge less likely.

## Worked lifecycle examples (each maps to a fixture in `merge_pr/consolidate` tests)

1. **Overlapping files**: #10 and #12 both edit `daemon/src/run.rs`, neither held. Prepare ⇒ attempt `cons-a1b2c3d4`, candidate PR #99 (body maps both, pins heads, carries the recorded rationale), reservations `after=#99` on both. Judge approves #99's combined diff; CI green; the pre-merge pin check passes; #99 merges via `merge-pr.sh`; the ordering pass releases both reservations on `CLEAR`; reconcile verifies ancestry, posts statuses, closes #10/#12 with the merge SHA (whether or not the pass has released them yet), closes their `Closes:` issues, deletes `loom/consolidated/cons-a1b2c3d4`.
2. **Semantic dependency**: #20 grows a fake-set, #21 dedups it — order matters (#21 after #20). E7 allows in-group ordering; construction merges in the recorded order; the ADR's position: consolidation preserves the ordering pass's landing order inside the candidate; a semantic dependency *across* an eligibility boundary (predecessor outside the group) blocks E7 until it lands.
3. **Substantive rejection**: during `candidate_open`, Judge rejects the combined diff (`changes-requested`). The candidate is **not** repaired in place, because any push to it aborts the attempt (§3). The Champion or operator runs `consolidate-abort`. If a Doctor fix, a merge of `main`, or any other commit is pushed to `loom/consolidated/cons-…` anyway, the next reconciliation touch sees the candidate head differ from the ledger and aborts on its own. In both cases: #99 is closed, this attempt's live reservations are released (the ones the pass already voided are skipped), and the branch is deleted. The fix belongs on the source PR(s), which are back to ordinary PRs. If consolidating them is still worthwhile after that, it is a fresh attempt with a new id. Nothing is written to the sources beyond their release, and they stay open.
4. **Held / workflow PRs**: #30 carries `loom:operator-only`, #31 edits `.github/workflows/ci.yml`. Both fail E3/E4 at eligibility, before any mutation; the verb reports each rejection reason and does nothing.
5. **Head push during preparation**: #40's head moves after pins are read but before the reservations are confirmed. Preparation re-reads every source head after applying reservations, and the mismatch is a hard abort under the same push-abort rule (§3). There is no silent re-pin. The candidate is closed, the reservations are released, and the sources are otherwise untouched.
6. **Duplicate workers**: two daemons run `consolidate-prepare` for the same group concurrently. Same attempt id ⇒ both converge: one pushes/creates, the other adopts the existing branch/PR and skips re-posting markers (exact-marker idempotency). No duplicate candidate.
7. **Failed candidate CI**: #99's combined CI is red. The candidate is not mergeable (existing guards). The operator/Champion runs `consolidate-abort`: candidate PR closed, reservations released, sources preserved and actionable exactly as before the attempt. Pushing a CI fix to the candidate is not an alternative, because the push itself aborts (§3). No auto-evict/retry in v1; a bounded retry is a fresh attempt with a new id.
8. **Interruption during component closure**: reconcile crashes after closing #10 but before #12. Restart: candidate merged (step 1 verify idempotent), #10's status exists (skip), #12's status posted, #12 closed, issues closed, branch deleted. Reconciliation never releases a reservation itself. The ordering pass released both on `CLEAR`, whether before, during or after the crash, and the crash does not affect that. Every step's precondition is re-read; nothing double-applies.
9. **Source push while the candidate is open**: #12's author pushes while #99 waits for review. On its next tick the ordering pass voids #12's reservation (`FollowerMoved`). The next reconciliation touch of #99 finds #12's live head differs from its pin and aborts: #99 is closed, #10's reservation (still live) is released, #12's is skipped (already voided), and the branch is deleted. If #99 had reached the merge path first, the pre-merge pin check would have refused it. The next ordering pass re-plans #10 and #12 from their current heads.

## Consequences

### Positive

- Fleet-scale repair churn from overlapping landing becomes schedulable without any new merge-path mechanism.
- Abandoned attempts self-heal (expiry) and interrupted ones self-resume (idempotent reconciliation); no orphaned gating state.
- The candidate is judged on its own merits — the approval/CI surface never widens.

### Negative

- A combined review bundles risk: a rejection blocks all components until repaired or aborted (accepted; v1 has no eviction, and the abort path keeps sources actionable).
- Reservation release vs. reconciliation is a small race window. The ordering pass releases on landing, before reconciliation closes the sources. A released source may merge before it is closed, which is accepted: its pinned diff is already on main, and the closure is then a no-op. The pass may also re-plan the released sources among themselves with `seq-` holds, which is accepted as inert because reconciliation closes them whatever their label state.
- Push-abort makes an attempt fragile: one author push or one fix commit ends it, and a repair means a fresh attempt with a fresh review. This is accepted under the operator ruling (2026-10-01) as the simplest default-deny rule, because it closes the gap where a fix push to the candidate silently lifted every source hold. §7's abort-by-cause counts are the evidence for revisiting it.
- Between a push and the abort, the sources are briefly unreserved while the candidate is open (§3 "The window"). This is safe only because of the pre-merge pin check, which is not yet implemented (see Follow-ups).
- Two more verbs and a module in the already-large merge surface; accepted — the alternative (prompt-side convention) is exactly the unenforced-bytes failure mode this contract exists to avoid.

### Follow-ups

- **#9839: implement push-abort after `candidate_open`.** The code was read, not built or run, as of 2026-10-01. The merged ordering pass (#9707) voids each moved hold one at a time and never aborts the attempt. A candidate push lifts every reservation while the candidate stays open. A source push leaves the sibling reservations in place behind a stale candidate that can still merge. The open-PR trigger (`TRIGGER_OPEN_PRS`) also skips Phase 1, including the landing release, in repositories with two or fewer open PRs. The stacked #9744 aborts on a push only *during* preparation. The stacked #9745 still releases reservations from reconciliation, which this revision rules out. #9839 tracks the drift check and abort, the pre-merge pin check, the exclusion of candidates from ordering, running Phase 1 regardless of the trigger, and removing reconciliation's release step.

## Alternatives Considered

- **Per-component eviction/retry on candidate failure** — rejected for v1: an automatic evict/retry/bisect search multiplies CI cost and can silently drop a component; the explicit abort keeps sources actionable. Revisit only with measurement data demanding it.
- **A new `loom:reserved` blocking label** — rejected: duplicates #9378's gate with a second label to teach every merge path about; the sequencing-hold form reuses the existing refusal, release, and expiry machinery.
- **Atomic multi-PR closure** — impossible on the forge (each update is a separate fallible write); the resumable reconciliation is the contract's answer, not an approximation of a lost ideal.
- **Constructing the candidate from patches/cherry-picks** — rejected: merge commits of pinned heads make inclusion verifiable by ancestry (§6.2), which patch application does not preserve.

## References

- Related GitHub Issues: #9063 (epic), #9687 (this record), #9688 (preparation), #9689 (landing/reconciliation), #9378 (the gate), #9686 (ordering pass), #9372 (stacked children), #9416 (equivalence carryover — explicitly not used), #9161 (merge preconditions), #9839 (push-abort implementation follow-up), 2AMLogic/loom-ui#590 (measurement consumer)
- Related ADRs: ADR-0022 (merge commit default — the landing shape), ADR-0018 (Rust owns behavior, shell reaches it)
- Fixture home: `loom-daemon/src/merge_pr/consolidate/tests.rs` (#9688/#9689)
