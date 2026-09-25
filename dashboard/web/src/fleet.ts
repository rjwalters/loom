/**
 * Snapshot → view model.
 *
 * The `/api/fleet-state` payload is two loosely-coupled collections (`hosts`
 * keyed by id, `activeSweeps` as a flat list carrying `hostId`). Every view
 * wants them joined per host, so the join lives here — once, pure, and
 * testable without a DOM.
 *
 * Two joins that are easy to get wrong, and are pinned by tests:
 *
 * - **The host set is the union of both collections, not `hosts`' keys.** A
 *   host whose first pushed record was a `sweep.started` has live sweeps and
 *   no `hosts` entry at all (the Durable Object only creates one on
 *   `host.health`/`tokens.snapshot` — see `../../src/fleetState.ts`). Keying
 *   off `hosts` alone would silently hide a busy host.
 * - **Zero sweeps is a normal state, not an empty state.** An idle host is
 *   healthy and must still render its health/token panel.
 */

import { roleFailureLabel, secondsSince } from "./format";
import type {
  ActiveComputeJob,
  ActiveSweep,
  FleetSnapshot,
  HostEntry,
  HostHealthRecord,
  MissingHostState,
  ProviderPoolAggregate,
  RoleTickFailure,
  TokenAccount,
} from "./types";

/**
 * Fleet-wide `host.health.roles` totals, summed across every reporting host
 * (#5642) — see `aggregateRoleTicks`.
 */
export interface RoleTickAggregate {
  total: number;
  ok: number;
}

/**
 * How old a `host.health` / `tokens.snapshot` may be before it is shown as
 * stale. The daemon samples both every ~5 minutes
 * (`docs/deploy-runbook.md` §10), so 15 minutes is three missed samples — long
 * enough that a single dropped push or a batching delay is not an alarm, short
 * enough to notice a host that stopped reporting.
 */
export const STALE_AFTER_SEC = 15 * 60;

export type HostStatus =
  /** Reporting recently, and the token pool has healthy capacity left, and
   * the host is not refusing work. High load alone does not disqualify a
   * host from `ok` — see `isHostDistressed` below: the trigger is
   * refusing-work or pinned-at-zero-idle, not raw utilization. */
  | "ok"
  /** Reporting recently, but something needs a person: a role tick failing
   * repeatedly and recently, a provider's token pool with nothing left, or
   * dispatch suppressed by load Loom does not own — see `distressReason`.
   * Transient, self-healing conditions are `throttled` instead (#8832). */
  | "degraded"
  /** Reporting recently and healthy, but running constrained by design: the
   * daemon's own host-distress breaker is holding new dispatch while load
   * drains, or the token pool is running low but not empty (#8832). This is
   * the fleet protecting itself, not a fault — it is shown, never counted in
   * `FleetView.needsAttention`. */
  | "throttled"
  /** Last report is older than `STALE_AFTER_SEC`. */
  | "stale"
  /** Known only from `activeSweeps`, or from a `hosts` entry with neither
   * `health` nor `tokens` yet — nothing to assess. */
  | "unknown"
  /** Named by the backend's expected-host roster, holds an active ingest key,
   * and has never reported (Issue #8792/#8804). A refinement of `unknown`:
   * same absence of data, but the roster tells us the absence is *wrong*.
   * Counts toward `FleetView.needsAttention` — it is an incident. */
  | "missing"
  /** Named by the expected-host roster but with no active ingest key, so it
   * cannot report yet (Issue #8792/#8804). Also a refinement of `unknown`,
   * but a planning to-do rather than an outage — deliberately NOT counted in
   * `needsAttention`, and rendered distinctly from `missing`. */
  | "unprovisioned";

/** `true` for the two statuses that come from the expected-host roster rather
 * than from telemetry (Issue #8804) — the hosts with no `health`/`tokens`
 * entry to assess at all. */
export function isRosterMissingStatus(status: HostStatus): boolean {
  return status === "missing" || status === "unprovisioned";
}

export interface TokenSummary {
  /** The per-account rows, or `[]` for a public viewer, who is sent an
   * aggregate instead. `total` is the pool size either way — check
   * `hasAccountDetail` rather than `accounts.length` to tell "no accounts"
   * from "accounts withheld". */
  accounts: TokenAccount[];
  total: number;
  exhausted: number;
  /** Highest known `usage_fraction`, or `undefined` when no account reports
   * one. Deliberately not `0` — see `format.ts`'s unknown-is-not-zero rule. */
  peakUsage: number | undefined;
  /** False when this summary came from the public aggregate, so the
   * per-account table has nothing to render and should say why. */
  hasAccountDetail: boolean;
  /** The same pool, one slice per provider (`claude`, `codex`, …) in
   * first-seen order — what lets the card show Claude's and Codex's
   * availability independently instead of one blended figure. Empty when
   * `total` is 0. A row/aggregate from a daemon or backend that predates
   * per-provider pools collapses into a single `"claude"` slice. */
  providers: ProviderSummary[];
}

/** One provider's slice of a `TokenSummary`. */
export interface ProviderSummary {
  provider: string;
  total: number;
  exhausted: number;
  /** Highest known `usage_fraction` in this slice, or `undefined` when no
   * account in it reports one (Codex accounts never do — never `0`). */
  peakUsage: number | undefined;
}

/** The provider a token-account row belongs to — `"claude"` when the
 * emitting daemon predates per-provider pools and sent none. */
export function accountProvider(account: TokenAccount): string {
  return account.provider && account.provider.length > 0 ? account.provider : "claude";
}

export interface HostView {
  hostId: string;
  entry: HostEntry;
  sweeps: ActiveSweep[];
  tokens: TokenSummary;
  status: HostStatus;
  /** Why `status` is `"degraded"` or `"throttled"`, naming the specific
   * cause (token exhaustion, a named halt reason, a failing role) rather than
   * a generic badge — see `views/fleetOverview.ts`'s badge tooltip.
   * `undefined` for every other status. */
  degradedReason: string | undefined;
  /** Most recent of the health/tokens `updatedAt`s — the host's liveness
   * signal. `undefined` when it has never reported either. */
  lastReportAt: string | undefined;
  /** Seconds since `lastReportAt`, or `undefined`. */
  lastReportAgeSec: number | undefined;
  /** This host's live ephemeral-compute jobs, keyed by the `sweepId` they
   * were submitted from (Issue #8835) — the "subprocesses" each sweep row
   * renders beneath itself. Only sweeps in `sweeps` appear as keys; a job
   * whose sweep is not live on this host is not here (it stays in the
   * fleet-level `FleetView.unattributedCompute` list instead).
   *
   * Empty for every host on a snapshot whose emitters do not stamp
   * `sweepId`, which is what keeps a pre-#8835 fleet rendering exactly as
   * before. */
  computeBySweep: ReadonlyMap<string, ActiveComputeJob[]>;
}

export interface FleetView {
  hosts: HostView[];
  /** Hosts with a real `health`/`tokens` entry — `hosts.length` minus those
   * known only from `activeSweeps` (`status === "unknown"`, see the module
   * doc's "union" note). This, not `hosts.length`, is what the overview
   * headline's "N hosts" count uses (#5101): `hosts` deliberately still
   * includes sweep-only hosts (and their cards), so counting `hosts.length`
   * in the headline would tell an operator the fleet has more reporting
   * hosts than it does. Mirrors the equivalent fix in
   * `dashboard/src/publicPage.ts`'s `renderFleetOverview` (#5078).
   *
   * `totalSweeps` is deliberately NOT split the same way: unlike the public
   * page's flat table (which needed a second "unattributed sweeps" table to
   * keep a sweep-only host's sweeps discoverable), every sweep here already
   * renders under its own host's card — including a sweep-only host's own
   * card — so the per-card grouping already answers "whose sweep is this",
   * and splitting the headline number too would only add a second count to
   * reconcile against the cards below it.
   */
  reportingHosts: number;
  /** Roster-expected hosts rendered with `status === "missing"` — enrolled,
   * but never reported (Issue #8804). `0` on any snapshot without a
   * `missingHosts` field, which is what keeps a pre-#8792 backend's overview
   * identical to its pre-#8804 rendering. Counted separately from
   * `reportingHosts` on purpose: these hosts are, by definition, not
   * reporting — folding them into that number would overstate how much of the
   * fleet is actually pushing telemetry. */
  missingHosts: number;
  /** Roster-expected hosts rendered with `status === "unprovisioned"` — named
   * by the roster but never enrolled, so they *cannot* report (Issue #8804).
   * Split from `missingHosts` because the two need different operator action;
   * see `HostStatus`. */
  unprovisionedHosts: number;
  totalSweeps: number;
  /** Hosts in `stale`, `degraded`, or `missing` — the count the overview
   * headline shows. `"unknown"` hosts are excluded here (STATUS_ORDER treats
   * them as their own bucket) — see the "excludes sweep-only hosts" test in
   * `fleet.test.ts` — and so is `"unprovisioned"`, which is a provisioning
   * to-do rather than something going wrong (#8804). `"missing"` *is*
   * counted: a host the roster says is enrolled and that has never reported
   * is the exact incident the roster exists to surface. */
  needsAttention: number;
  /** Fleet-wide role-tick totals (#5642) — see `aggregateRoleTicks`.
   * `undefined` when no reporting host has sent `health.roles` yet. Exists
   * because `totalSweeps` alone reads as "the fleet is idle" whenever every
   * currently-running agent happens to be doing role work (Curator/Champion/
   * Judge/Doctor ticks) rather than a sweep — role ticks never post to
   * `activeSweeps`, so a fleet that is genuinely busy can legitimately show
   * `totalSweeps === 0`. This gives the overview headline a second, already-
   * exported signal to show alongside it so `0` is never mistaken for
   * "nothing is running". */
  roleTicks: RoleTickAggregate | undefined;
  /** Live `ephemeral_compute` jobs (Issue #8306), leaked-first then
   * longest-running — see `sortComputeJobs`.
   *
   * Deliberately **not** joined onto `hosts` the way `activeSweeps` is. A
   * compute job's `hostId` names the process that *reported* it, not a machine
   * the fleet manages: the reference emitter is a hostless elastic batch
   * runner authenticating as one synthetic id for the whole fleet (see
   * `defaults/docs/observability.md` §5d), so grouping by it would pile every
   * instance in the world under a single card that has no `host.health` to
   * render beside them. The jobs are a fleet-level list of their own instead.
   *
   * Issue #8835 refines that without contradicting it: a job that names the
   * sweep which submitted it (`sweepId`) *is* attributable, and renders nested
   * under that sweep as well. This list stays the fleet-wide total either way
   * — it is what the headline count and `leakedCompute` are derived from — and
   * `unattributedCompute` is the subset the flat "running compute" table
   * renders.
   */
  activeCompute: ActiveComputeJob[];
  /** The subset of `activeCompute` that could **not** be nested under a live
   * sweep (Issue #8835): no `sweepId` at all (an emitter predating the field,
   * or a submission from outside any sweep), or a `sweepId` naming no sweep in
   * `activeSweeps` (the sweep already finished, or its host stopped
   * reporting).
   *
   * These are exactly the jobs with no other home on the page, so this — not
   * `activeCompute` — is what the "running compute" table renders. An orphaned
   * or leaked instance can therefore never be hidden by the nesting: if it has
   * no live sweep to hide under, it is in this list. */
  unattributedCompute: ActiveComputeJob[];
  /** How many of `activeCompute` the backend flagged as leaked — an instance
   * still billing with no completion record. The count the overview headline
   * shows, so a leak is visible without scanning the list. Counted over the
   * whole fleet, nested and unattributed alike (#8835), so nesting a job under
   * its sweep can never quietly decrement the fleet's leak count. */
  leakedCompute: number;
}

/**
 * Sums `health.roles.total`/`.ok` across every host that has reported the
 * field (#5642) — the same per-host numbers `roleTickCompactText`/
 * `roleTickSummaryText` already render on each card, folded into one fleet-
 * wide count for the overview headline.
 *
 * Returns `undefined`, not `{ total: 0, ok: 0 }`, when no host has reported
 * `roles` at all (a pre-#5022 fleet, or every reporting daemon predates the
 * field) — that is "unknown", not "role ticks were sampled and there were
 * none", and the two must render differently (see `format.ts`'s
 * unknown-is-not-zero rule).
 */
export function aggregateRoleTicks(hosts: HostView[]): RoleTickAggregate | undefined {
  let total = 0;
  let ok = 0;
  let reported = false;
  for (const host of hosts) {
    const roles = host.entry.health?.record.roles;
    if (roles === undefined || roles.total === undefined) continue;
    reported = true;
    total += roles.total;
    ok += roles.ok ?? 0;
  }
  return reported ? { total, ok } : undefined;
}

/**
 * Normalize either `tokens.snapshot` shape into one `TokenSummary`.
 *
 * An authenticated viewer's record carries `accounts` and the totals are
 * derived from it. A public viewer's record carries the server-computed
 * aggregate instead (`../../src/redaction.ts`'s `deriveTokenPoolAggregate`)
 * and the same totals are read straight off it. Views get identical fields
 * either way — only the per-account table needs to know the difference, via
 * `hasAccountDetail`.
 */
export function summarizeTokens(entry: HostEntry): TokenSummary {
  const record = entry.tokens?.record;
  const accounts = record?.accounts;

  if (accounts) {
    let peakUsage: number | undefined;
    let exhausted = 0;
    const providers: ProviderSummary[] = [];
    for (const account of accounts) {
      if (account.exhausted) exhausted += 1;
      if (account.usage_fraction !== undefined) {
        peakUsage = peakUsage === undefined ? account.usage_fraction : Math.max(peakUsage, account.usage_fraction);
      }
      const name = accountProvider(account);
      let slice = providers.find((entry) => entry.provider === name);
      if (!slice) {
        slice = { provider: name, total: 0, exhausted: 0, peakUsage: undefined };
        providers.push(slice);
      }
      slice.total += 1;
      if (account.exhausted) slice.exhausted += 1;
      if (account.usage_fraction !== undefined) {
        slice.peakUsage =
          slice.peakUsage === undefined ? account.usage_fraction : Math.max(slice.peakUsage, account.usage_fraction);
      }
    }
    return { accounts, total: accounts.length, exhausted, peakUsage, hasAccountDetail: true, providers };
  }

  const total = record?.account_count ?? 0;
  const exhausted = record?.exhausted_count ?? 0;
  const peakUsage = record?.max_usage_fraction ?? undefined;
  return {
    accounts: [],
    total,
    exhausted,
    peakUsage,
    // A host that has simply never sent a tokens.snapshot has no record at
    // all; a public viewer's record exists but withholds the rows.
    hasAccountDetail: record === undefined,
    providers: record?.providers
      ? record.providers.map(providerSummaryFromAggregate).filter((slice): slice is ProviderSummary => slice !== undefined)
      : // A backend that predates the per-provider aggregate: the whole
        // pool was the Claude pool.
        total > 0
        ? [{ provider: "claude", total, exhausted, peakUsage }]
        : [],
  };
}

/** One public `providers[]` slice → `ProviderSummary`; `undefined` for a
 * slice too malformed to name (no `provider`), which is dropped rather than
 * rendered under a fabricated name. */
function providerSummaryFromAggregate(slice: ProviderPoolAggregate): ProviderSummary | undefined {
  if (!slice.provider) return undefined;
  return {
    provider: slice.provider,
    total: slice.account_count ?? 0,
    exhausted: slice.exhausted_count ?? 0,
    peakUsage: slice.max_usage_fraction ?? undefined,
  };
}

/**
 * A pool is treated as "close to the edge" — worth a `throttled` badge (#8832;
 * `degraded` before it) — once one account or fewer is left to rotate onto,
 * or three quarters of the pool is spent. Below that line, some accounts
 * being exhausted is the pool working exactly as designed (the selector
 * rotates away from them), not a fault: see #4864.
 */
const LOW_AVAILABILITY_THRESHOLD = 1;
const HIGH_EXHAUSTION_FRACTION = 0.75;

/** `true` when the token pool is empty of capacity or nearly so. A pool with
 * no reported accounts (`total === 0`) is not flagged by this check — that
 * host simply has not sent a `tokens.snapshot` yet. */
export function isTokenPoolDegraded(tokens: TokenSummary): boolean {
  return degradedProviders(tokens).length > 0;
}

/** The provider pools that are at or near exhaustion, by name — the
 * `throttled` case (#8832); `emptyProviders` above is the `degraded` one.
 * Each provider is judged on its own: a fleet whose Claude pool is spent
 * cannot dispatch Claude sweeps no matter how many Codex accounts sit idle,
 * so one blended availability figure would hide exactly the outage an
 * operator needs to see. A summary with no provider slices (an empty pool)
 * falls back to the pool-wide numbers, which is then also empty — not
 * flagged. */
export function degradedProviders(tokens: TokenSummary): string[] {
  const slices = tokens.providers.length > 0
    ? tokens.providers
    : [{ provider: "claude", total: tokens.total, exhausted: tokens.exhausted, peakUsage: tokens.peakUsage }];
  return slices.filter((slice) => isPoolSliceDegraded(slice.total, slice.exhausted)).map((slice) => slice.provider);
}

/** The provider pools with no account left to rotate onto at all — the
 * token condition that actually stops dispatch, and so the only one that
 * makes a host `degraded` (#8832). A pool merely *near* exhaustion
 * (`degradedProviders`) is the selector working as designed during a weekly
 * wall and renders `throttled`. */
export function emptyProviders(tokens: TokenSummary): string[] {
  const slices = tokens.providers.length > 0
    ? tokens.providers
    : [{ provider: "claude", total: tokens.total, exhausted: tokens.exhausted, peakUsage: tokens.peakUsage }];
  return slices.filter((slice) => slice.total > 0 && slice.exhausted >= slice.total).map((slice) => slice.provider);
}

function isPoolSliceDegraded(total: number, exhausted: number): boolean {
  if (total === 0) return false;
  const available = total - exhausted;
  if (available === 0) return true;
  // "One account left to rotate onto" is only a warning sign for a pool
  // that had more: a single-account provider (one Codex subscription) is
  // its normal, healthy self at one available, not perpetually degraded.
  if (total > 1 && available <= LOW_AVAILABILITY_THRESHOLD) return true;
  return exhausted / total >= HIGH_EXHAUSTION_FRACTION;
}

/**
 * The daemon's own host-distress-breaker trip threshold
 * (`DEFAULT_HOST_BREAKER_LOAD_PER_CORE`, `loom-daemon/src/host_breaker.rs`) —
 * reused verbatim rather than reinvented, so this UI's notion of "distressed"
 * can never drift from the daemon's own. A host that merely trips this once
 * is not automatically flagged: `dispatch_halted` (below) is the primary,
 * *sustained* signal — this raw threshold is a same-number fallback for a
 * daemon build old enough, or configured, to not send `dispatch_halted` at
 * all.
 */
const HOST_DISTRESS_LOAD_PER_CORE = 2.5;

/**
 * `cpu_idle_fraction` at or below this reads as "pinned at zero", not merely
 * busy — a build spike can dip idle into the teens without the host refusing
 * work, so this only fires once idle is close enough to true zero that the
 * host looks like it cannot breathe (see #4975: 0% idle / load-per-core 4.24
 * was the incident that motivated this check).
 */
const ZERO_IDLE_FRACTION_THRESHOLD = 0.02;

/**
 * How many failed ticks a `(root, role)` pair must show in the daemon's
 * role-tick ring before the dashboard calls the host degraded (#8832). The
 * daemon lists a pair as `persistent` the moment its *latest* tick failed —
 * a single failure — and with ~50 repos × 8 roles per host, some pair has
 * nearly always just failed once. Three is above a one-off blip (a slow
 * forge, a token rotation) while still well under the daemon's own
 * five-in-a-row escalation (`ROLE_TICK_ESCALATION_THRESHOLD`).
 */
export const ROLE_FAILURE_DEGRADED_MIN = 3;

/**
 * How recent a failing pair's latest tick must be to count (#8832). The
 * daemon samples its whole tick ring with no window, so a pair that failed
 * and then stopped ticking (repo unregistered, role disabled) stays
 * "persistent" until eviction or restart. Two hours is twice the longest
 * built-in role interval (architect, 3600 s): a pair that should have ticked
 * again by now and has not is history, not a live fault.
 */
export const ROLE_FAILURE_RECENT_SEC = 2 * 60 * 60;

/**
 * The admission brake's starving-on-foreign-load halt (#8478) — the one
 * `dispatch_halted` cause that is an incident rather than backpressure: the
 * host refuses all work because of load Loom does not own, and nothing in
 * Loom will clear it. Matched on the daemon's own reason prefix
 * (`dispatch_halt_from_breaker`, `loom-daemon/src/observability/collector.rs`).
 */
const FOREIGN_LOAD_HALT = /^admission brake STARVING\b/;

/** The persistent role failures that are both repeated and recent — the only
 * ones that make a host `degraded` (#8832). */
export function sustainedRoleFailures(
  health: HostHealthRecord | undefined,
  now: Date = new Date(),
): RoleTickFailure[] {
  return (health?.roles?.persistent ?? []).filter((failure) => {
    if ((failure.failures ?? 0) < ROLE_FAILURE_DEGRADED_MIN) return false;
    const age = secondsSince(failure.last_at, now);
    // No timestamp: cannot show it is stale, so do not hide it.
    return age === undefined || age <= ROLE_FAILURE_RECENT_SEC;
  });
}

/**
 * Why a host needs a person, in priority order, or `undefined` when it does
 * not. Narrowed by #8832 so `degraded` means "act on this", not "something
 * somewhere ticked badly once":
 *
 * - dispatch halted by the admission brake on foreign load (#8478);
 * - a role pair failing repeatedly and recently (`sustainedRoleFailures`);
 * - the single-sample load/idle heuristics — ONLY for a record with no
 *   `dispatch_halted` field at all (a daemon that predates #4975). When the
 *   daemon reports `dispatch_halted`, its breaker's *sustained* verdict is
 *   authoritative and one hot 5-minute sample is not a second opinion.
 *
 * The host-distress breaker's own halt is deliberately NOT here: it is the
 * host shedding load as designed, and renders `throttled` via
 * `throttleReason`. A merely busy host stays `ok` (busy ≠ degraded, #4975).
 */
export function distressReason(health: HostHealthRecord | undefined, now: Date = new Date()): string | undefined {
  if (!health) return undefined;
  if (health.dispatch_halted && health.halt_reason && FOREIGN_LOAD_HALT.test(health.halt_reason)) {
    return `dispatch halted: ${health.halt_reason}`;
  }
  const sustained = sustainedRoleFailures(health, now);
  if (sustained.length > 0) {
    const names = sustained.map(roleFailureLabel).join(", ");
    return `role tick(s) persistently failing: ${names}`;
  }
  if (health.dispatch_halted === undefined) {
    if (health.load_per_core !== undefined && health.load_per_core >= HOST_DISTRESS_LOAD_PER_CORE) {
      return `load/core ${health.load_per_core.toFixed(2)} at or above the host-distress threshold (${HOST_DISTRESS_LOAD_PER_CORE})`;
    }
    if (health.cpu_idle_fraction !== undefined && health.cpu_idle_fraction <= ZERO_IDLE_FRACTION_THRESHOLD) {
      return "CPU idle pinned near zero";
    }
  }
  return undefined;
}

/** Why a healthy host is holding back new work by its own design (#8832) —
 * the host-distress breaker (or any non-foreign-load halt) — or `undefined`.
 * Checked only after `distressReason` found nothing. */
export function throttleReason(health: HostHealthRecord | undefined): string | undefined {
  if (!health?.dispatch_halted) return undefined;
  return health.halt_reason ? `dispatch paused: ${health.halt_reason}` : "dispatch paused";
}

/** `true` when `distressReason` finds a reason — see its doc for the rules. */
export function isHostDistressed(health: HostHealthRecord | undefined, now: Date = new Date()): boolean {
  return distressReason(health, now) !== undefined;
}

/** Newest of the two `updatedAt`s. String compare is safe here *only* because
 * both are backend-generated `new Date().toISOString()` values — fixed-width
 * UTC, so lexicographic order is chronological order. */
function latestReport(entry: HostEntry): string | undefined {
  const candidates = [entry.health?.updatedAt, entry.tokens?.updatedAt].filter(
    (value): value is string => typeof value === "string" && value.length > 0,
  );
  if (candidates.length === 0) return undefined;
  return candidates.sort()[candidates.length - 1];
}

/**
 * @param rosterState When set, this host is named by the backend's
 *   expected-host roster and has no telemetry at all (Issue #8804) — so its
 *   status becomes the roster's own verdict (`missing`/`unprovisioned`)
 *   instead of the generic `unknown`. Only honored when the host has in fact
 *   never reported; a host with any `health`/`tokens` entry keeps its
 *   telemetry-derived status, so a card can never say "never reported"
 *   alongside a live "last report 2m ago".
 */
export function buildHostView(
  hostId: string,
  entry: HostEntry,
  sweeps: ActiveSweep[],
  now: Date = new Date(),
  rosterState?: MissingHostState,
  computeBySweep: ReadonlyMap<string, ActiveComputeJob[]> = new Map(),
): HostView {
  const tokens = summarizeTokens(entry);
  const lastReportAt = latestReport(entry);
  const lastReportAgeSec = secondsSince(lastReportAt, now);
  const distress = distressReason(entry.health?.record, now);

  let status: HostStatus;
  let degradedReason: string | undefined;
  if (lastReportAgeSec === undefined) {
    status = rosterState ?? "unknown";
  } else if (lastReportAgeSec > STALE_AFTER_SEC) {
    status = "stale";
  } else if (distress !== undefined) {
    // Host-level distress takes priority over token-pool exhaustion when
    // both fire — it is the more actionable/urgent of the two reasons.
    status = "degraded";
    degradedReason = distress;
  } else if (emptyProviders(tokens).length > 0) {
    status = "degraded";
    const empty = emptyProviders(tokens);
    degradedReason = `${empty.join(", ")} token pool${empty.length === 1 ? "" : "s"} exhausted — nothing left to dispatch on`;
  } else {
    // #8832: self-protective, self-clearing conditions — shown, not alarmed.
    const throttle = throttleReason(entry.health?.record);
    if (throttle !== undefined) {
      status = "throttled";
      degradedReason = throttle;
    } else if (isTokenPoolDegraded(tokens)) {
      status = "throttled";
      const low = degradedProviders(tokens);
      degradedReason = `${low.join(", ")} token pool${low.length === 1 ? "" : "s"} running low`;
    } else {
      status = "ok";
    }
  }

  return {
    hostId,
    entry,
    sweeps,
    tokens,
    status,
    degradedReason,
    lastReportAt,
    lastReportAgeSec,
    computeBySweep,
  };
}

/** Sort: hosts needing attention first, then busiest, then by id so the list
 * does not reshuffle between polls when nothing changed. `missing` leads —
 * a host the roster says should be reporting and that never has is the
 * loudest signal on the page — while `unprovisioned` sits just above the
 * data-less `unknown` bucket, since it is a planning to-do (#8804). The
 * relative order of the four pre-existing statuses is unchanged. */
const STATUS_ORDER: Record<HostStatus, number> = {
  missing: 0,
  stale: 1,
  degraded: 2,
  throttled: 3,
  unprovisioned: 4,
  unknown: 5,
  ok: 6,
};

/**
 * Split live compute jobs into "nests under a live sweep" and "does not"
 * (Issue #8835).
 *
 * **The join key is `sweepId` alone — never `hostId`.** A compute job's
 * `hostId` is the *submitter's* ingest identity; the reference emitter is a
 * hostless elastic batch runner authenticating as one synthetic id for its
 * whole fleet, so it routinely differs from the host the submitting sweep runs
 * on. Falling back to a `hostId` match would attribute a job to whatever
 * sweeps happen to share that synthetic identity — i.e. confidently wrong,
 * which is worse here than unattributed.
 *
 * **Nothing is dropped.** Every job lands in exactly one of the two outputs,
 * so a job whose sweep is finished, never started, or simply never stamped —
 * precisely the shape an orphaned, still-billing instance takes — is
 * guaranteed to appear in the flat "running compute" list, leaked flag and
 * all. That is the whole safety property of this function, and it is what its
 * tests pin.
 *
 * `jobs` is expected pre-sorted (`sortComputeJobs`); both outputs preserve
 * that order.
 */
export function attributeComputeJobs(
  jobs: readonly ActiveComputeJob[],
  sweeps: readonly ActiveSweep[],
): { bySweep: Map<string, ActiveComputeJob[]>; unattributed: ActiveComputeJob[] } {
  const liveSweepIds = new Set(sweeps.map((sweep) => sweep.sweepId));
  const bySweep = new Map<string, ActiveComputeJob[]>();
  const unattributed: ActiveComputeJob[] = [];
  for (const job of jobs) {
    const sweepId = job.sweepId;
    if (sweepId === undefined || sweepId.length === 0 || !liveSweepIds.has(sweepId)) {
      unattributed.push(job);
      continue;
    }
    const list = bySweep.get(sweepId);
    if (list) list.push(job);
    else bySweep.set(sweepId, [job]);
  }
  return { bySweep, unattributed };
}

export function buildFleetView(snapshot: FleetSnapshot, now: Date = new Date()): FleetView {
  const sweepsByHost = new Map<string, ActiveSweep[]>();
  for (const sweep of snapshot.activeSweeps) {
    const list = sweepsByHost.get(sweep.hostId);
    if (list) list.push(sweep);
    else sweepsByHost.set(sweep.hostId, [sweep]);
  }

  // Issue #8804: the roster-expected hosts the backend reported as having no
  // `health` entry (#8792). They join the host set exactly the way sweep-only
  // hosts do — the whole point is that a host which never reported must be
  // *visible*, not silently absent from a page built only from what did
  // report.
  //
  // An entry is ignored when that host turns out to carry a `health` or
  // `tokens` record after all. `diffExpectedRoster` already excludes
  // health-bearing hosts, so this only fires on a hand-built/malformed
  // snapshot or on the tokens-only edge the backend's own diff does not look
  // at — and in both cases the telemetry is the better answer: overriding it
  // would paint a host that demonstrably reported minutes ago as "never
  // reported".
  const rosterStates = new Map<string, MissingHostState>();
  for (const missing of snapshot.missingHosts ?? []) {
    const entry = snapshot.hosts[missing.hostId];
    if (entry?.health || entry?.tokens) continue;
    rosterStates.set(missing.hostId, missing.state);
  }

  const hostIds = new Set<string>([
    ...Object.keys(snapshot.hosts),
    ...sweepsByHost.keys(),
    ...rosterStates.keys(),
  ]);

  // `activeCompute` is absent on a snapshot parsed from a pre-#8305 backend
  // and on any hand-built fixture that predates this field.
  const activeCompute = sortComputeJobs(snapshot.activeCompute ?? []);
  // Issue #8835: attribute each job to the sweep that submitted it, by
  // `sweepId` alone. Unattributed jobs keep their fleet-level list.
  const { bySweep: computeBySweep, unattributed: unattributedCompute } = attributeComputeJobs(
    activeCompute,
    snapshot.activeSweeps,
  );

  const hosts = [...hostIds]
    .map((hostId) =>
      buildHostView(
        hostId,
        snapshot.hosts[hostId] ?? {},
        sortSweeps(sweepsByHost.get(hostId) ?? []),
        now,
        rosterStates.get(hostId),
        // Narrow the fleet-wide map to this host's own sweeps so a card never
        // has to reason about a sweep it does not render.
        hostComputeBySweep(computeBySweep, sweepsByHost.get(hostId) ?? []),
      ),
    )
    .sort(
      (a, b) =>
        STATUS_ORDER[a.status] - STATUS_ORDER[b.status] ||
        b.sweeps.length - a.sweeps.length ||
        a.hostId.localeCompare(b.hostId),
    );

  return {
    hosts,
    reportingHosts: hosts.filter((host) => host.status !== "unknown" && !isRosterMissingStatus(host.status)).length,
    missingHosts: hosts.filter((host) => host.status === "missing").length,
    unprovisionedHosts: hosts.filter((host) => host.status === "unprovisioned").length,
    totalSweeps: snapshot.activeSweeps.length,
    needsAttention: hosts.filter(
      (host) => host.status === "stale" || host.status === "degraded" || host.status === "missing",
    ).length,
    roleTicks: aggregateRoleTicks(hosts),
    activeCompute,
    unattributedCompute,
    leakedCompute: activeCompute.filter((job) => job.leaked === true).length,
  };
}

/** The slice of the fleet-wide `sweepId → jobs` map belonging to one host's
 * own sweeps (Issue #8835). Returns an empty map when this host has no sweep
 * with nested jobs, which is the overwhelmingly common case. */
function hostComputeBySweep(
  bySweep: ReadonlyMap<string, ActiveComputeJob[]>,
  sweeps: readonly ActiveSweep[],
): ReadonlyMap<string, ActiveComputeJob[]> {
  const scoped = new Map<string, ActiveComputeJob[]>();
  for (const sweep of sweeps) {
    const jobs = bySweep.get(sweep.sweepId);
    if (jobs && jobs.length > 0) scoped.set(sweep.sweepId, jobs);
  }
  return scoped;
}

/** Leaked jobs first (they are the ones costing money with nobody watching),
 * then longest-running, then by `jobId` so the list does not reshuffle between
 * polls when nothing changed. Mirrors `sortSweeps`' tie-breaking, including
 * its "a job with no start time sorts last" rule — an unparseable or absent
 * `startedAt` must not masquerade as the oldest job. */
export function sortComputeJobs(jobs: readonly ActiveComputeJob[]): ActiveComputeJob[] {
  const startKey = (job: ActiveComputeJob): number => {
    const parsed = job.startedAt ? Date.parse(job.startedAt) : Number.POSITIVE_INFINITY;
    return Number.isNaN(parsed) ? Number.POSITIVE_INFINITY : parsed;
  };
  return [...jobs].sort(
    (a, b) =>
      Number(b.leaked === true) - Number(a.leaked === true) ||
      startKey(a) - startKey(b) ||
      a.jobId.localeCompare(b.jobId),
  );
}

/** Longest-running first (a sweep with no `startedAt` sorts last), then by
 * `sweepId` for a stable order across polls. */
export function sortSweeps(sweeps: ActiveSweep[]): ActiveSweep[] {
  return [...sweeps].sort((a, b) => {
    const aStart = a.startedAt ? Date.parse(a.startedAt) : Number.POSITIVE_INFINITY;
    const bStart = b.startedAt ? Date.parse(b.startedAt) : Number.POSITIVE_INFINITY;
    const aKey = Number.isNaN(aStart) ? Number.POSITIVE_INFINITY : aStart;
    const bKey = Number.isNaN(bStart) ? Number.POSITIVE_INFINITY : bStart;
    return aKey - bKey || a.sweepId.localeCompare(b.sweepId);
  });
}

export function findHost(view: FleetView, hostId: string): HostView | undefined {
  return view.hosts.find((host) => host.hostId === hostId);
}
