---
name: loom-judge-claims
description: "**Load when**: the candidate PR already carries `loom:reviewing` (another Judge may hold it fresh, or its process may have died mid-review)."
---
<!-- loom-managed-skill -->
<!-- GENERATED FILE — DO NOT EDIT DIRECTLY.
     Produced by `loom-daemon generate-agent-skills` from
     defaults/.claude/commands/loom/judge-claims.md (the same source Claude Code
     reads as /loom:judge-claims via .claude/commands/loom/). This is the
     cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)
     read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see
     runtime-adapters.md §5. To change this file, edit the source above
     and re-run the generator; CI (`loom-daemon generate-agent-skills
     --check`) fails if this file is stale. -->

# Judge: Stale `loom:reviewing` Claim Check (Step 2)

**Load when**: the candidate PR already carries `loom:reviewing` (another Judge may hold it fresh, or its process may have died mid-review).

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

### Stale `loom:reviewing` Claim Check (Step 2)

Run this **before** claiming a PR in step 2 above. `gh pr list
--label="loom:review-requested"` can surface a PR that another Judge already
claimed (`loom:review-requested` and `loom:reviewing` coexist while a review
is in progress) — including one whose claiming Judge's process died mid-review
(parent sweep crash). Without this check, that dead claim blocks the PR from
ever being reviewed again. This is the minutes-scale analog of the
`loom:building` staleness convention (`LOOM_STALE_BUILDING_HOURS`,
`loom-daemon/src/claim_reconciliation.rs`) — reviews run 5–15 minutes in
practice, not hours, so the grace period is minutes, not hours.

**If the PR does NOT carry `loom:reviewing`:** claim it (step 2).

**If the PR DOES carry `loom:reviewing`:** evaluate the claim with the shared
staleness evaluator. **Do not hand-roll the timeline/comment arithmetic** —
`judge.md`, `doctor.md` and `curator.md` all drive the same script so the three
lanes cannot drift apart, and it is unit-tested
(`.loom/scripts/tests/test-claim-staleness.sh`, #6514):

```bash
N=<pr-number>
# Every read the script makes is a live `gh api` call, never "$GH_READ": this is
# claim arbitration, and a 30s-stale timeline or comment list is exactly the
# window in which a competing Judge's claim (or its stand-down) lands.
eval "$(./.loom/scripts/claim-staleness.sh check --number "$N" --label loom:reviewing)"
echo "$CLAIM_STATE — claim age ${CLAIM_AGE_MINUTES}m, idle ${IDLE_MINUTES}m, stand-down streak ${STANDDOWN_COUNT}"
```

`eval` is safe here: the script emits only `KEY=VALUE` lines built from a fixed
enum, validated RFC3339 timestamps and integers — never forge text — so no
comment body can reach your shell. Use `--json` instead if you prefer `jq`.

Then decide on `$CLAIM_STATE`:

| `$CLAIM_STATE` | Meaning | Action |
|---|---|---|
| `unclaimed` | the PR does not actually carry `loom:reviewing` right now | Claim it (step 2). |
| `fresh` | a Judge is plausibly still working this PR | **Do not stomp the claim.** Record a stand-down (see below), then skip this PR and continue the batch to the next candidate PR. |
| `stale` | no *claimant* activity for ≥ `LOOM_STALE_REVIEWING_MINUTES` (default **45**) — the claiming Judge's process almost certainly died mid-review | Reclaim (see below), then proceed with the normal review from step 3. |
| `stale-bounded-fallback` | the stand-down streak reached `LOOM_MAX_STANDDOWN_STREAK` (default **3**) **and** the claim's own age is ≥ `LOOM_STALE_REVIEWING_MINUTES` | Force-reclaim (see below) — the livelock breaker. |
| `unknown` | a timeline/label read failed, or its markers could not be authenticated (#9548) | **Fail safe: treat exactly like `fresh`.** Never stomp a claim on API failure or missing data. |

**What counts as claimant activity (#6514)**: only a trusted author's (#9548) comment carrying *this
claim's* activity marker —

```
<!-- loom:claim-activity claim=$CLAIMED_AT -->
```

Every other comment is ignored: it neither pins nor extends the claim (the PR #6513 livelock: a
routine Builder post-push note used to pin a claim "fresh" forever). Claimant activity only **resets
the idle clock**, so a genuine heartbeat buys another
`LOOM_STALE_REVIEWING_MINUTES`.

The script also asks the daemon (`forge claim-liveness`, #10235) for a trusted Judge-progress comment
(`loom:verdict-sha`, `loom:review-reconciliation`, ...; not `loom:ac-verified`, which Builders also post) or your own force-push of the PR head, so both sides agree;
without that verb only the marker below counts.

**If your review runs long** (esp. before a CI wait), post a progress comment ending
with that marker so the next pass can see you are alive. The script prints the
marker for the live claim, so you never hand-assemble it:

```bash
gh pr comment $N --body "Judge: still running the test suite (step 7) — review in progress.
$(./.loom/scripts/claim-staleness.sh marker --number "$N" --label loom:reviewing)"
```

**Stand-down marker convention (#4618)**: a "standing down, not stomping"
comment is evidence of **no activity** — a *later* pass declined to touch the
claim. Stand-down comments carry their own marker and count into the streak,
never into liveness (counting them made claims look eternally fresh, PR #4614).

**Recording a stand-down** (the `fresh` and `unknown` rows):

```bash
./.loom/scripts/claim-staleness.sh standdown --number "$N" --label loom:reviewing
# then skip this PR and continue the batch
```

It posts the marked stand-down comment the first time, and on every later pass
**edits that same comment in place**, bumping `seq=` in its marker:

```
<!-- loom:standdown claim=$CLAIMED_AT seq=2 -->
```

**This keeps the bounded fallback reachable (#6514)**: the streak accumulates
without near-identical comments (#5123). A legacy marker with no `seq=` counts as
`seq=1`. Staleness is still re-verified on **every** pass.

**Bounded fallback (AC3, #4618/#4798/#6514)**: the streak counts stand-down
passes against *this exact* `$CLAIMED_AT` (a genuine reclaim resets it). A
streak alone measures peer arrival rate, not liveness (#4790), so the fallback
fires only once **both** hold: `LOOM_MAX_STANDDOWN_STREAK` passes against the
same claim, **and** the claim's own age (not the idle clock, so a looping
claimant cannot hold it forever) is ≥ `LOOM_STALE_REVIEWING_MINUTES`. Use this
reclaim comment:

```bash
# The lease first: a live peer lease (exit 4) means stand down, label untouched.
[[ -n "$CLAIMED_AT" ]] && loom-daemon lease pr claim $N --role judge --watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}"; [[ $? =~ ^[02]$ ]] &&
gh pr edit $N --remove-label "loom:reviewing" &&
gh pr comment $N --body "Reclaiming loom:reviewing claim: $STANDDOWN_COUNT stand-down passes against claim $CLAIMED_AT (age ≥ ${LOOM_STALE_REVIEWING_MINUTES:-45}m) with no actual review progress (bounded fallback, LOOM_MAX_STANDDOWN_STREAK=${LOOM_MAX_STANDDOWN_STREAK:-3}) — breaking the livelock." &&
gh pr edit $N --add-label "loom:reviewing"
# Continue to step 3 (Check merge state) and evaluate normally
```

**Reclaiming a stale claim** (the ordinary idle-clock path):

```bash
# The lease first: a live peer lease (exit 4) means stand down, label untouched.
[[ -n "$CLAIMED_AT" ]] && loom-daemon lease pr claim $N --role judge --watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}"; [[ $? =~ ^[02]$ ]] &&
gh pr edit $N --remove-label "loom:reviewing" &&
gh pr comment $N --body "Reclaiming stale loom:reviewing claim (idle ${IDLE_MINUTES}m > ${LOOM_STALE_REVIEWING_MINUTES:-45}m with no claimant activity) — the prior Judge likely died mid-review." &&
gh pr edit $N --add-label "loom:reviewing"
# Continue to step 3 (Check merge state) and evaluate normally
```

**Env vars**: `LOOM_STALE_REVIEWING_MINUTES` (default **45**) — named to
mirror `LOOM_STALE_BUILDING_HOURS` (`loom-daemon/src/claim_reconciliation.rs`,
the analogous no-record staleness threshold for `loom:building` claims), but
on a **minutes**, not hours, scale, since review turnaround (5–15 minutes) is
two orders of magnitude faster than a build. `LOOM_MAX_STANDDOWN_STREAK`
(default **3**) — the AC3 bounded-fallback cap described above. The script
reads both itself; `--stale-minutes` / `--max-standdown-streak` override them.

**If `.loom/scripts/claim-staleness.sh` is missing** (an older install that has
not been resynced yet): fall back to the **age-only** rule — read the latest
`labeled` event for `loom:reviewing` from
`repos/{owner}/{repo}/issues/$N/timeline`, reclaim when it is older than
`LOOM_STALE_REVIEWING_MINUTES`, and treat a failed or empty read as fresh. Do
**not** reintroduce a "any comment after the claim means fresh" test — that is
precisely the defect this section exists to fix.

**Daemon backstop (#4367/#4618)**: `loom-daemon`'s `claim_reconciliation`
pass also reclaims stale `loom:reviewing` (and `loom:treating`) on its tick,
with the same env vars and defaults, aged from the label's own `labeled`
event. See [`daemon-reference.md`'s "Stale-claim reconciliation"
section](https://github.com/rjwalters/loom/blob/main/defaults/docs/daemon-reference.md#stale-claim-reconciliation--the-sweep-journal-3953-fixed-3975-extended-to-pr-side-claims-4367).

**Applies everywhere a Judge claims a PR from a multi-PR pass** — not just
this single-PR narrative. This same check-then-claim rule governs the batch
loop in "Autonomous mode (configured with targetInterval)" under Completion
below, and any cron-invoked pass over `loom:review-requested` PRs: a
cron-invoked Judge and a `/loom:sweep`-dispatched Judge must apply the
identical rule so neither stomps the other's fresh claim nor stalls behind a
dead one.
