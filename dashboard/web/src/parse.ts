/**
 * Wire JSON → `types.ts` narrowing.
 *
 * Everything the UI renders passes through here first. The rules are the same
 * three the telemetry schema doc mandates for any consumer:
 *
 * - **Unknown is not zero.** A missing or wrong-typed measurement is dropped,
 *   never coerced to `0` — `host.health` omits fields whose probe failed, and
 *   rendering a failed CPU probe as "0% idle" would invent an alarm.
 * - **Additive fields are tolerated.** An unrecognized key is ignored, never
 *   fatal, so a host on a newer daemon still renders on an older dashboard.
 * - **Anything that is not exactly `"public"` is private.** Same fail-safe
 *   decode the Rust side and the Worker both implement.
 *
 * A malformed *envelope* (not an object, `hosts` not an object, `activeSweeps`
 * not an array) degrades to the empty parts of the snapshot rather than
 * throwing, so one bad sub-tree cannot blank the whole page.
 */

import type {
  ActiveComputeJob,
  ActiveSweep,
  FleetSnapshot,
  HostEntry,
  HostHealthRecord,
  HostProtection,
  ManagedRepoEntry,
  MissingHost,
  MissingHostState,
  ProviderPoolAggregate,
  RoleTickFailure,
  RoleTickHealth,
  TokenAccount,
  TokensSnapshotRecord,
  Timestamped,
} from "./types";
import type { LabelsSnapshotRecord } from "./labelTypes";
import { parseLabelsSnapshot } from "./labelParse";
import { parseQueueSnapshot } from "./queueParse";

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function str(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

/** Finite numbers only. `NaN`/`Infinity` survive `JSON.parse` of nothing, but
 * they do arrive from a hand-rolled producer, and they format as garbage. */
function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function bool(value: unknown): boolean | undefined {
  return typeof value === "boolean" ? value : undefined;
}

/** An array of non-empty strings, dropping any wrong-typed/empty entry
 * rather than failing the whole field — same best-effort narrowing as every
 * scalar helper above. `undefined` (not `[]`) when `value` itself is not an
 * array, so an absent field stays absent rather than becoming a fabricated
 * empty list. */
function strArray(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) return undefined;
  return value.filter((entry): entry is string => typeof entry === "string" && entry.length > 0);
}

/** A `managed_repos` entry. `slug` is dropped by `stripUndefined` when
 * wrong-typed, or when the backend has already redacted it away (a private
 * repo, unauthenticated viewer) — see `ManagedRepoEntry`'s doc.
 * `visibility` always defaults to `"private"` on anything but the exact
 * string `"public"` — the same fail-safe-default `ActiveSweep.visibility`
 * already applies, never left `undefined`. */
export function parseManagedRepoEntry(value: unknown): ManagedRepoEntry | undefined {
  if (!isObject(value)) return undefined;
  return stripUndefined<ManagedRepoEntry>({
    slug: str(value.slug),
    visibility: value.visibility === "public" ? "public" : "private",
  });
}

/** A `roles.persistent` entry. `root`/`role` degrade to `undefined` (not a
 * fabricated empty string) when wrong-typed, matching every other
 * best-effort field this module narrows. */
export function parseRoleTickFailure(value: unknown): RoleTickFailure | undefined {
  if (!isObject(value)) return undefined;
  return stripUndefined<RoleTickFailure>({
    root: str(value.root),
    role: str(value.role),
    failures: num(value.failures),
    last_at: str(value.last_at),
    detail: str(value.detail),
  });
}

/** `host.health`'s `roles` summary (#5022). A genuine `total: 0` (the role
 * runner sampled nothing this snapshot) survives untouched — `num()` only
 * drops a missing or wrong-typed value, never a real zero. */
export function parseRoleTickHealth(value: unknown): RoleTickHealth | undefined {
  if (!isObject(value)) return undefined;
  return stripUndefined<RoleTickHealth>({
    total: num(value.total),
    ok: num(value.ok),
    persistent: Array.isArray(value.persistent)
      ? value.persistent.map(parseRoleTickFailure).filter((entry): entry is RoleTickFailure => entry !== undefined)
      : undefined,
  });
}

/** `host.health`'s `protection` summary (#5352). `state` degrades to
 * `undefined` (not a fabricated string) when wrong-typed, matching every
 * other best-effort field this module narrows — the consuming view must then
 * treat it the same as a record from a pre-#5352 daemon: "not reported",
 * never "unprotected". */
export function parseHostProtection(value: unknown): HostProtection | undefined {
  if (!isObject(value)) return undefined;
  return stripUndefined<HostProtection>({
    state: str(value.state),
    watchdog_provisioned: bool(value.watchdog_provisioned),
  });
}

export function parseHostHealth(value: unknown): HostHealthRecord {
  if (!isObject(value)) return {};
  return stripUndefined<HostHealthRecord>({
    kind: str(value.kind),
    captured_at: str(value.captured_at),
    daemon_version: str(value.daemon_version),
    build_commit: str(value.build_commit),
    built_at: str(value.built_at),
    uptime_sec: num(value.uptime_sec),
    logical_cpus: num(value.logical_cpus),
    cpu_idle_fraction: num(value.cpu_idle_fraction),
    load_per_core: num(value.load_per_core),
    worktree_root_free_gb: num(value.worktree_root_free_gb),
    worktree_root_total_gb: num(value.worktree_root_total_gb),
    dispatch_halted: bool(value.dispatch_halted),
    halt_reason: str(value.halt_reason),
    managed_repos: Array.isArray(value.managed_repos)
      ? value.managed_repos.map(parseManagedRepoEntry).filter((entry): entry is ManagedRepoEntry => entry !== undefined)
      : undefined,
    roles: parseRoleTickHealth(value.roles),
    protection: parseHostProtection(value.protection),
    is_captain: bool(value.is_captain),
    armed_singleton_jobs: strArray(value.armed_singleton_jobs),
  });
}

export function parseTokenAccount(value: unknown): TokenAccount {
  if (!isObject(value)) return {};
  return stripUndefined<TokenAccount>({
    account: str(value.account),
    provider: str(value.provider),
    rank: num(value.rank),
    usage_fraction: num(value.usage_fraction),
    limit_window_reset_at: str(value.limit_window_reset_at),
    exhausted: bool(value.exhausted),
  });
}

export function parseTokensSnapshot(value: unknown): TokensSnapshotRecord {
  if (!isObject(value)) return {};
  return stripUndefined<TokensSnapshotRecord>({
    kind: str(value.kind),
    captured_at: str(value.captured_at),
    accounts: Array.isArray(value.accounts) ? value.accounts.map(parseTokenAccount) : undefined,
    // The public aggregate that stands in for `accounts` — see
    // `TokensSnapshotRecord`'s doc. `num` drops nulls, which is what the
    // backend sends for "no account reported one".
    account_count: num(value.account_count),
    exhausted_count: num(value.exhausted_count),
    mean_usage_fraction: num(value.mean_usage_fraction),
    max_usage_fraction: num(value.max_usage_fraction),
    next_limit_window_reset_at: str(value.next_limit_window_reset_at),
    providers: Array.isArray(value.providers) ? value.providers.map(parseProviderPoolAggregate) : undefined,
  });
}

export function parseProviderPoolAggregate(value: unknown): ProviderPoolAggregate {
  if (!isObject(value)) return {};
  return stripUndefined<ProviderPoolAggregate>({
    provider: str(value.provider),
    account_count: num(value.account_count),
    exhausted_count: num(value.exhausted_count),
    max_usage_fraction: num(value.max_usage_fraction),
    next_limit_window_reset_at: str(value.next_limit_window_reset_at),
  });
}

function parseTimestamped<T>(value: unknown, parseRecord: (raw: unknown) => T): Timestamped<T> | undefined {
  if (!isObject(value)) return undefined;
  return { record: parseRecord(value.record), updatedAt: str(value.updatedAt) ?? "" };
}

export function parseActiveSweep(value: unknown): ActiveSweep | undefined {
  if (!isObject(value)) return undefined;
  const hostId = str(value.hostId);
  const sweepId = str(value.sweepId);
  // Both are structurally guaranteed by the Durable Object (`hostId` comes
  // from the authenticated key, `sweepId` is the storage key itself). An entry
  // missing either is not addressable in the UI — it could not be attributed
  // to a host card or keyed in a list — so it is dropped rather than rendered
  // under a fabricated identity.
  if (!hostId || !sweepId) return undefined;
  return stripUndefined<ActiveSweep>({
    hostId,
    sweepId,
    repo: str(value.repo),
    // Fail-safe: only the exact string "public" is public.
    visibility: value.visibility === "public" ? "public" : "private",
    issue: num(value.issue),
    phase: str(value.phase),
    startedAt: str(value.startedAt),
    enteredPhaseAt: str(value.enteredPhaseAt),
    model: str(typeof value.model === "string" ? value.model.trim() : value.model),
    effort: str(value.effort),
    runtime: str(typeof value.runtime === "string" ? value.runtime.trim() : value.runtime),
    provider: str(typeof value.provider === "string" ? value.provider.trim() : value.provider),
    updatedAt: str(value.updatedAt),
  });
}

/** Narrow one `activeCompute` entry (Issue #8305/#8306).
 *
 * `hostId`/`jobId` are structurally guaranteed by the Durable Object, and an
 * entry missing either is unaddressable in the UI — it could not be listed
 * under a stable key or attributed to an emitter — so it is dropped rather
 * than rendered under a fabricated identity, exactly as
 * {@link parseActiveSweep} drops a sweep with no `sweepId`.
 *
 * `leaked` decodes only the literal `true` to `true`. A backend predating
 * Issue #8305 omits the field entirely, and "unknown" must render as
 * *not flagged* — the opposite default would paint every job on an older
 * deployment as a leak. */
export function parseActiveComputeJob(value: unknown): ActiveComputeJob | undefined {
  if (!isObject(value)) return undefined;
  const hostId = str(value.hostId);
  const jobId = str(value.jobId);
  if (!hostId || !jobId) return undefined;
  return stripUndefined<ActiveComputeJob>({
    hostId,
    jobId,
    // Issue #8835. Unlike `hostId`/`jobId` this is optional and non-fatal: a
    // job with no `sweepId` is perfectly addressable, it simply has no sweep
    // to nest under and stays in the flat "running compute" list.
    sweepId: str(value.sweepId),
    instanceId: str(value.instanceId),
    region: str(value.region),
    instanceType: str(value.instanceType),
    spot: bool(value.spot),
    ami: str(value.ami),
    startedAt: str(value.startedAt),
    updatedAt: str(value.updatedAt),
    leaked: value.leaked === true ? true : undefined,
  });
}

const MISSING_HOST_STATES: readonly MissingHostState[] = ["missing", "unprovisioned"];

/** Narrow one `missingHosts` entry (Issue #8792 backend → #8804 SPA).
 *
 * Strict on both fields, unlike the best-effort narrowing the `host.health`
 * record gets. An entry with no `hostId` is unaddressable in the UI — it could
 * not be listed under a stable key or linked to a drill-down — so it is
 * dropped rather than rendered under a fabricated identity, exactly as
 * {@link parseActiveSweep} drops a sweep with no `sweepId`.
 *
 * An unrecognized `state` is dropped for a different reason: the two known
 * states prescribe *opposite* operator actions (`missing` — investigate a host
 * that should be reporting; `unprovisioned` — enroll a host that never was),
 * so guessing one for an unknown third value would tell an operator to do the
 * wrong thing. The usual "a newer producer must still render" concern does not
 * apply here the way it does to `host.health`: `missingHosts` is computed by
 * the Worker (`../../src/index.ts`), which ships from the same commit and the
 * same deploy as this bundle — not by N independently-updated host daemons. A
 * backend that grows a third state therefore lands together with the dashboard
 * build that knows how to render it. */
export function parseMissingHost(value: unknown): MissingHost | undefined {
  if (!isObject(value)) return undefined;
  const hostId = str(value.hostId);
  if (!hostId) return undefined;
  const state = MISSING_HOST_STATES.find((known) => known === value.state);
  if (!state) return undefined;
  return { hostId, state };
}

export function parseFleetSnapshot(value: unknown): FleetSnapshot {
  const snapshot: FleetSnapshot = { hosts: {}, activeSweeps: [], activeCompute: [] };
  if (!isObject(value)) return snapshot;

  if (isObject(value.hosts)) {
    for (const [hostId, raw] of Object.entries(value.hosts)) {
      if (!isObject(raw)) continue;
      const entry: HostEntry = {};
      const health = parseTimestamped(raw.health, parseHostHealth);
      if (health) entry.health = health;
      const tokens = parseTimestamped(raw.tokens, parseTokensSnapshot);
      if (tokens) entry.tokens = tokens;
      // Issue #8852: a queue with no parseable `tick_at` is dropped, not shown.
      const queue = parseTimestamped(raw.queue, parseQueueSnapshot);
      if (queue?.record) entry.queue = { record: queue.record, updatedAt: queue.updatedAt };
      // Issue #9094: a snapshot with no repo or `taken_at` cannot be diffed.
      if (Array.isArray(raw.labels)) {
        const labels = raw.labels
          .map((item) => parseTimestamped(item, parseLabelsSnapshot))
          .filter((item): item is Timestamped<LabelsSnapshotRecord> => item?.record !== undefined);
        if (labels.length > 0) entry.labels = labels;
      }
      snapshot.hosts[hostId] = entry;
    }
  }

  if (Array.isArray(value.activeSweeps)) {
    for (const raw of value.activeSweeps) {
      const sweep = parseActiveSweep(raw);
      if (sweep) snapshot.activeSweeps.push(sweep);
    }
  }

  // Absent on a `/public/fleet-state` response's older shape and on any
  // backend predating Issue #8305 — both degrade to the empty list, same as
  // every other malformed sub-tree above.
  const activeCompute: ActiveComputeJob[] = [];
  if (Array.isArray(value.activeCompute)) {
    for (const raw of value.activeCompute) {
      const job = parseActiveComputeJob(raw);
      if (job) activeCompute.push(job);
    }
  }
  snapshot.activeCompute = activeCompute;

  // Issue #8804. Deliberately NOT normalized to `[]` when absent, unlike
  // `activeCompute` above: an omitted field means "this backend has no roster
  // notion at all" (pre-#8792, or no `EXPECTED_HOSTS` configured), and leaving
  // the key off keeps every consumer — and every pre-existing fixture's
  // `toEqual` — byte-identical to its pre-#8804 behavior. A wrong-typed
  // `missingHosts` degrades the same way, since it is then not an array.
  if (Array.isArray(value.missingHosts)) {
    snapshot.missingHosts = value.missingHosts
      .map(parseMissingHost)
      .filter((host): host is MissingHost => host !== undefined);
  }

  return snapshot;
}

/** Drop explicitly-`undefined` keys so `"uptime_sec" in record` stays an
 * honest "the daemon sent it" test rather than "the key was constructed". */
function stripUndefined<T extends object>(value: Record<string, unknown>): T {
  for (const key of Object.keys(value)) {
    if (value[key] === undefined) delete value[key];
  }
  return value as T;
}
