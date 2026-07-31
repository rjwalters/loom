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

export function parseHostHealth(value: unknown): HostHealthRecord {
  if (!isObject(value)) return {};
  return stripUndefined<HostHealthRecord>({
    kind: str(value.kind),
    captured_at: str(value.captured_at),
    daemon_version: str(value.daemon_version),
    uptime_sec: num(value.uptime_sec),
    logical_cpus: num(value.logical_cpus),
    cpu_idle_fraction: num(value.cpu_idle_fraction),
    load_per_core: num(value.load_per_core),
    worktree_root_free_gb: num(value.worktree_root_free_gb),
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
