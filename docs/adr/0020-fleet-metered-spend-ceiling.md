# ADR-0020: Govern a Shared Metered API Key With a Provider-Side Spend Ceiling, Not Fleet Aggregation

## Status

Accepted

## Context

Loom's ordered runtime preference (#8436, ADR-adjacent: `runtime_preference`)
lets a host fall through from a flat-rate Claude/Codex subscription to a
**metered** OpenAI-compatible endpoint reached through a native harness, as a
backstop. The operator clarification on #8436 (2026-09-20) then named a
structural gap, filed as #8556:

> A metered OpenAI-compatible endpoint key is **one credential shared across
> many fleet hosts**, while every credential-governance mechanism Loom has is
> host-local.

Concretely, all three existing mechanisms are per host:

| Mechanism | Scope |
|---|---|
| `.loom/tokens/` Claude OAuth pool | one host's pool directory |
| The #8401 API-key account pool | one host's `.loom/api-keys/` (or the machine-shared root) |
| The #8555 metered-tap concurrency ceiling | one host's in-flight count |

So **N hosts each honouring a local ceiling of K still permit N×K concurrent
metered launches**, and nothing in the fleet knows the aggregate. A per-host cap
is not a spend cap. #8555 scopes the fleet-wide case out of itself and points
here.

The failure mode is asymmetric and that asymmetry drives the decision: too
permissive spends real money with no bound; too strict idles a fleet whose paid
subscriptions were already sitting there. A mechanism that is *approximately*
right in the permissive direction is not a ceiling.

#8556 laid out two options.

## Decision

**Option 1 — a provider-side hard spend ceiling — is the primary mechanism.
Option 2 (fleet aggregation through the observability backend) is a scoped
fallback, filed as a separate issue and built only if a chosen provider turns
out not to expose a ceiling that *fails the launch*.**

Two things follow, and they are deliberately separable:

1. **The prerequisite lands now, independent of either option**: usage
   accounting carries the **tap** — `(runtime, credential source)` — so "how
   much went to the metered backstop vs. the subscriptions" is a query rather
   than a reconstruction. This is `loom-daemon/src/tap_usage.rs`, the `tap`
   field on the `# LOOM_LAUNCH` record, `OutcomeRecord::tap_usage`,
   `config["tap"]` on the `sweep.outcome` telemetry record, and
   `loom-daemon sweep-outcomes summary --group-by tap`.
2. **The ceiling itself is not code in this repository under Option 1.** It is
   an account-level control configured at the metered provider and verified to
   reject a launch past the limit. That makes it an operator/mechanical task
   against a live credential, not a `loom-daemon` feature — which is why it is
   tracked as its own issue rather than bundled with the accounting.

**Verification bar for Option 1, stated so it cannot be quietly weakened**: the
ceiling must be observed to *fail the launch*, not to alert after the fact. A
provider control that only emails a threshold notice is **not** a ceiling under
this decision; discovering that is exactly the trigger that promotes Option 2
from fallback to the active mechanism.

**If Option 2 is ever built**, this ADR pre-commits its two hardest choices so
they are not re-litigated under time pressure:

- **Fail-closed on the metered tap specifically**, never on the whole dispatch
  path. An unreachable backend must not stall Claude/Codex work that has nothing
  to do with the metered credential.
- **Bounded staleness, not indefinite trust.** A counter read older than a
  configured window is treated as *unavailable* (and therefore fails the metered
  tap closed), not as a current reading. An unbounded "last known good" counter
  is fail-open wearing a fail-closed label.

## Consequences

### Positive

- **The authoritative mechanism is the one that can actually be authoritative.**
  Only the provider can refuse to serve a request; a fleet-side counter can only
  ask hosts to cooperate.
- **No new eventual-consistency window.** Option 2's counter is, by
  construction, behind reality by up to one aggregation interval — a burst across
  N hosts inside that window still overshoots. Option 1 has no such window.
- **No new dispatch-path dependency.** Option 2 couples the decision "may this
  host select the metered tap?" to a network service; Option 1 does not.
- **The prerequisite is valuable on its own**, and ships whether or not either
  ceiling is ever built: it answers the backstop-vs-subscription spend question,
  and it is what Option 2 would have to consume anyway.

### Negative

- **Option 1 is contingent on the provider.** If a chosen metered provider
  exposes only alerting, this decision degrades to "detect after the fact" until
  Option 2 is built. The mitigation is the explicit verification bar above plus
  the pre-filed fallback issue, not optimism.
- **Option 1 lives outside the repository.** A provider-side setting is not in
  version control, has no test, and can be changed or lost without a diff. Its
  only in-repo trace is the verification record on its issue.
- **Tap-attributed accounting is an estimate, not an invoice.** Per
  `defaults/docs/runtime-model-trials.md`, a harness cost estimate is
  directionally meaningful for a metered tap and not a charge at all for a
  flat-rate one, and a missing counter means unmeasured rather than zero. The
  accounting is therefore a fleet-visibility instrument, never a billing
  reconciliation — and must not be presented as one.
- **Choosing provider-side means the fleet still cannot answer "what is the
  aggregate right now?" itself.** It only learns after the fact, from the
  accounting. That is accepted: the ceiling's job is to *stop* spend, and the
  accounting's job is to *explain* it.

## Alternatives Considered

- **Option 2 as the primary mechanism (fleet aggregation through the
  observability backend).** Rejected as primary, retained as fallback. It is
  eventually consistent by construction, it introduces a new failure mode whose
  fail-open form defeats the entire purpose and whose fail-closed form couples
  dispatch availability to backend uptime, and it bounds spend only
  approximately — while costing materially more code than a provider setting. It
  becomes correct the moment its premise holds: that no provider-side hard
  ceiling exists.
- **Extending the #8555 per-host ceiling to a smaller K.** Rejected: it is the
  same host-local mechanism with worse throughput. N×K shrinks but stays
  unbounded in N, and the fleet's host count is not a constant.
- **A shared lock/lease on the metered credential (e.g. a forge-mediated or
  filesystem-mediated fleet lease).** Rejected: it bounds *concurrency*, which
  is not what a metered key bills on. Two long, expensive launches can outspend
  ten short cheap ones, so a concurrency lease is not a spend ceiling either —
  it is #8555 with more moving parts and a new cross-host liveness problem.
- **Deriving a ceiling from the harness's own cost estimates and holding the tap
  when the running total crosses it.** Rejected as the mechanism (kept as the
  reporting layer): `runtime-model-trials.md` is explicit that a harness estimate
  is not a measured charge, so enforcing real-money policy on it would make
  Loom's spend bound only as trustworthy as an unverified number the harness
  volunteered.

## References

- Related GitHub Issues: #8556 (this decision), #8436 (ordered runtime
  preference and the operator clarification behind it), #8555 (the per-host
  concurrency ceiling that scopes the fleet-wide case out of itself), #8401 /
  #8447 (the API-key account pool and its secret-free credential attribution)
- Related documentation: `defaults/docs/runtime-model-trials.md` (the
  estimate-is-not-a-charge rule this ADR inherits),
  `defaults/docs/observability.md` (the backend Option 2 would aggregate
  through), `defaults/docs/telemetry-schema.md` (`config.tap` and the
  `tap_*` usage keys)
- Implementation of the prerequisite: `loom-daemon/src/tap_usage.rs`,
  `loom-daemon/src/launch_record.rs` (`TapAttribution`)
