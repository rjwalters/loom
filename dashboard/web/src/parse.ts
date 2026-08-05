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
  ActiveSweep,
  FleetSnapshot,
  HostEntry,
  HostHealthRecord,
  HostProtection,
  ManagedRepoEntry,
  RoleTickFailure,
  RoleTickHealth,
  TokenAccount,
  TokensSnapshotRecord,
  Timestamped,
} from "./types";

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
  });
}

export function parseTokenAccount(value: unknown): TokenAccount {
  if (!isObject(value)) return {};
  return stripUndefined<TokenAccount>({
    account: str(value.account),
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
    model: str(value.model),
    effort: str(value.effort),
    updatedAt: str(value.updatedAt),
  });
}

export function parseFleetSnapshot(value: unknown): FleetSnapshot {
  const snapshot: FleetSnapshot = { hosts: {}, activeSweeps: [] };
  if (!isObject(value)) return snapshot;

  if (isObject(value.hosts)) {
    for (const [hostId, raw] of Object.entries(value.hosts)) {
      if (!isObject(raw)) continue;
      const entry: HostEntry = {};
      const health = parseTimestamped(raw.health, parseHostHealth);
      if (health) entry.health = health;
      const tokens = parseTimestamped(raw.tokens, parseTokensSnapshot);
      if (tokens) entry.tokens = tokens;
      snapshot.hosts[hostId] = entry;
    }
  }

  if (Array.isArray(value.activeSweeps)) {
    for (const raw of value.activeSweeps) {
      const sweep = parseActiveSweep(raw);
      if (sweep) snapshot.activeSweeps.push(sweep);
    }
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
