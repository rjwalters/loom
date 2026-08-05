/**
 * Visibility-based response redaction — the single policy layer between the
 * unclassified query API (`src/query.ts`, issue #4726) and any client (Epic
 * #4702, Phase 2, issue #4727).
 *
 * **The single enforcement point.** Every `/api/*` (authenticated) and
 * `/public/*` (public) route handler in `src/index.ts` calls into this
 * module as a post-processing step over `query.ts`'s already-unclassified
 * results — `query.ts` itself is intentionally unmodified (still returns
 * full detail for every kind, per its own module doc) so there is exactly
 * one place that decides what a viewer is allowed to see. Nothing else in
 * this backend — the D1 queries, the Durable Object, the live-tail poll
 * loop — implements any redaction of its own.
 *
 * ## How "authenticated" is determined
 *
 * Route-based split (see `docs/query-api.md` and `docs/cloudflare-access.md`
 * for the full rationale): the existing unprefixed `/api/*` routes are the
 * **authenticated** surface — an operator's Cloudflare Access policy is
 * expected to gate everything except the paths `docs/cloudflare-access.md`
 * already documents as Bypass (`/ingest`, `/public`, `/admin/*`). The new
 * `/public/*` routes mirror `/api/*` 1:1 and are always redacted, matching
 * the `/public` path Cloudflare Access's Bypass policy already reserves.
 * `src/index.ts` passes a plain `isAuthenticated: boolean` into every
 * function here based purely on *which route matched* — there is no
 * JWT/header parsing anywhere in this Worker (the epic's explicit
 * constraint: "no auth code in the dashboard itself"). The Worker trusts
 * "this request reached this path" as the authentication signal, exactly as
 * it already trusts a Custom Domain request having traversed Access at all
 * (see `docs/cloudflare-access.md` §5 — even signature verification of the
 * injected `Cf-Access-Jwt-Assertion` header is explicitly deferred, "not
 * implemented today").
 *
 * ## Redaction policy: per-kind field allowlist, not a blocklist
 *
 * For a **private**-visibility record viewed **without** authentication, the
 * policy is an **allowlist** of fields known today to be safe (counts,
 * rates, durations, lifecycle metadata) — never a blocklist of fields known
 * today to be unsafe. This is deliberate defense-in-depth: a blocklist only
 * protects against leak vectors this module's author thought of; an
 * allowlist protects against every leak vector too, including a future
 * schema field (e.g. an issue title, once one exists on the wire) that gets
 * added to `.loom/docs/telemetry-schema.md` without this module being
 * updated in lockstep. An unrecognized `kind` (forward-compatible per the
 * schema doc's `schema_version` contract) gets the most conservative
 * allowlist of all: `kind` only.
 *
 * ## `tokens.snapshot` / `host.health` are host-level, not repo-level
 *
 * These two kinds carry no `repo`/`visibility` field at all — Phase 1's
 * `decodeVisibility` (`src/telemetry.ts`) therefore stores them as
 * `visibility: "private"` by its fail-safe-default rule (a **storage**
 * classification, not a statement that their content is repo-identifying).
 * Neither references a repository, issue, branch, or PR — the four leak
 * vectors the epic's acceptance criteria name.
 *
 * `host.health` is mostly pure capacity telemetry (CPU/uptime/disk) and
 * passes through in full for every viewer. The one exception is its
 * `managed_repos` roster (#4976): each entry names a specific repository, so
 * — like `tokens.snapshot`'s `accounts` below — it is deliberately absent
 * from the allowlist and instead redacted through `redactManagedRepos`: a
 * public entry's slug survives, a private entry's slug is dropped but the
 * entry itself is kept (so the roster's size, and therefore "how many are
 * private", stays visible without naming any of them).
 *
 * `tokens.snapshot` does **not**. Its `accounts` array names the pool's
 * accounts and gives each one's rank and burn — operational detail about
 * *who* runs the fleet, which is a different question from how loaded it is.
 * Operator decision (2026-07-31): the authenticated dashboard shows the
 * per-account rows; the public view gets `deriveTokenPoolAggregate`'s
 * non-identifying summary instead (pool size, exhausted count, mean/peak
 * usage, next reset). Before that decision `/public/fleet-state` served the
 * whole array to anyone.
 *
 * Both are enforced through the same allowlist mechanism as every other kind
 * (not a special-cased bypass), so a future field added to either is dropped
 * by default until the table is deliberately updated — the fail-safe
 * direction never flips silently.
 */

import type { RepoVisibility } from "./telemetry";
import { decodeVisibility } from "./telemetry";
import type { HistoryQueryResult, HistoryRecord } from "./query";
import type { ActiveSweepState, FleetSnapshot } from "./fleetState";

// ---------------------------------------------------------------------------
// Per-kind field allowlist for the nested `record` payload
// ---------------------------------------------------------------------------

/** The nested-payload fields that survive into a public, unauthenticated
 * response for a **private**-visibility record of this `kind`. Every field
 * omitted here is a deliberate redaction, not an oversight — see the module
 * doc for the allowlist-not-blocklist rationale. Keep this table in sync
 * with `.loom/docs/telemetry-schema.md`'s "Record kinds" section: a new
 * field on an existing kind does NOT appear in a public response until it is
 * added here on purpose. */
const RECORD_FIELD_ALLOWLIST: Readonly<Record<string, readonly string[]>> = {
  "sweep.started": ["kind", "started_at", "model", "effort"],
  "sweep.phase": ["kind", "phase", "entered_at"],
  "sweep.completed": ["kind", "completed_at", "result"],
  "sweep.outcome": ["kind", "model", "effort", "config", "phase_durations", "total_duration_sec", "result"],
  // `pr_number` is deliberately ABSENT above (the PR-link leak vector), and
  // so are the four work-output fields issue #5357 added — `tokens_in`,
  // `tokens_out`, `lines_added`, `lines_deleted`. Per that issue's own text,
  // token/LOC counts are "workload detail" for a private repo (how much was
  // spent/changed on it), the same category of detail `pr_number` is held
  // back for, so they stay behind the same allowlist boundary rather than
  // getting a bespoke exception.
  // Host-level kinds: no repo/issue/branch/PR reference exists on either —
  // see the module doc's "tokens.snapshot / host.health" section. Every
  // field the schema defines today is listed explicitly (not "pass
  // everything") so the allowlist mechanism stays the single source of
  // truth even for these two kinds.
  //
  // `accounts` is deliberately ABSENT here: the per-account rows carry
  // `account` identifiers and per-account burn, which the public view
  // summarizes instead of listing (see `PUBLIC_RECORD_DERIVATIONS`).
  "tokens.snapshot": ["kind", "captured_at"],
  "host.health": [
    "kind",
    "captured_at",
    "daemon_version",
    // Build identity of the emitting binary (#4956). Reviewed for redaction
    // and deliberately allowed through: a short commit SHA and a build
    // timestamp describe the released open-source binary, carry no repo /
    // issue / branch / operator reference, and are exactly as sensitive as
    // the `daemon_version` directly above them.
    "build_commit",
    "built_at",
    "uptime_sec",
    "logical_cpus",
    "cpu_idle_fraction",
    "load_per_core",
    "worktree_root_free_gb",
    // Total capacity (GB) of the worktree-root scratch volume (#5356):
    // reviewed for redaction and deliberately allowed through, same
    // reasoning as `worktree_root_free_gb` directly above it — a disk-size
    // figure describes the machine, not any repo, operator, or workload.
    // Capacity alone is a mild fingerprinting signal for a named host (it
    // narrows which physical machine a report came from), but free-GB is
    // already public and this is the denominator that turns it into a
    // percentage — a decision this issue makes explicitly, not left open.
    "worktree_root_total_gb",
    // Dispatch-attention state (#4975): whether this host's own dispatch is
    // currently halted (host-distress breaker / saturation-hold / rate-limit
    // breaker) and why. Neither names a repo, issue, branch, or operator —
    // same "describes the machine, not the work" reasoning as the fields
    // directly above.
    "dispatch_halted",
    "halt_reason",
    // Watchdog/crash-protection state (#5352): whether this host's daemon
    // has crash protection armed and whether a watchdog is provisioned to
    // restart it. Neither names a repo, issue, branch, or operator — same
    // "describes the machine, not the work" reasoning as `dispatch_halted`/
    // `halt_reason` directly above.
    "protection",
    // `managed_repos` (#4976) is deliberately ABSENT here: each entry names a
    // specific repository, so — like `tokens.snapshot`'s `accounts` above —
    // it only ever reaches a public response through `PUBLIC_RECORD_
    // DERIVATIONS`'s `redactManagedRepos`, never a raw copy.
    //
    // Role-tick health (`roles`, #5022) is likewise deliberately ABSENT here:
    // each `persistent[]` entry carries a `root` — a full absolute filesystem
    // workspace path whose home-directory segment names the *operator* on the
    // common macOS/Linux layout (`/Users/<user>/…`, `/home/<user>/…`), the
    // same category of "who runs the fleet" detail `tokens.snapshot`'s
    // `accounts` is held back for. So — like `managed_repos` — it only ever
    // reaches a public response through `PUBLIC_RECORD_DERIVATIONS`'s
    // `redactRoleTickHealth`, which basenames each `root` (mirroring the
    // daemon's `RoleFailure::label()` and the frontend's `pathBasename`),
    // never a raw copy. The authenticated `/api/*` surface keeps the full
    // path (that path skips redaction entirely).
  ],
};

/** One account row inside a `tokens.snapshot`, as the daemon sends it. Every
 * measured field is optional: the "unknown != zero" contract says an
 * unmeasurable probe is absent, never a fake `0`. */
interface TokenAccountRow {
  account?: unknown;
  rank?: unknown;
  usage_fraction?: unknown;
  limit_window_reset_at?: unknown;
  exhausted?: unknown;
}

/** The non-identifying summary the public view gets in place of the
 * per-account rows. Field-for-field, this answers "how loaded is the pool"
 * without answering "which accounts exist" or "which one is nearly spent". */
export interface TokenPoolAggregate {
  account_count: number;
  exhausted_count: number;
  /** Mean/peak across the accounts that actually reported a
   * `usage_fraction`; `null` when none did (never a misleading `0`). */
  mean_usage_fraction: number | null;
  max_usage_fraction: number | null;
  /** Earliest limit-window reset across the pool — a fleet-level "capacity
   * returns at" that names no account. `null` when none reported one. */
  next_limit_window_reset_at: string | null;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

/** Round to 4 decimal places, enough resolution for a percentage readout
 * without exposing float noise that could fingerprint an exact input. */
function round4(value: number): number {
  return Math.round(value * 10_000) / 10_000;
}

/**
 * Summarize a `tokens.snapshot`'s `accounts` array into
 * {@link TokenPoolAggregate}.
 *
 * Operator decision (2026-07-31): the authenticated dashboard shows
 * per-account token detail; the public view shows aggregate stats only. The
 * pool's account identifiers (`agent5-2amlogic`, …), their individual burn
 * rates, and their ranking are all operational detail about *who* is running
 * the fleet, so they stay behind the Access gate. Total/peak load and how
 * many accounts are spent describe the fleet's capacity, which is the part
 * worth showing publicly.
 *
 * Defensive against a malformed payload: a missing or non-array `accounts`
 * yields a zero-count aggregate rather than throwing, because this runs on
 * the response path of a live SSE stream.
 */
export function deriveTokenPoolAggregate(payload: Record<string, unknown>): TokenPoolAggregate {
  const rows: TokenAccountRow[] = Array.isArray(payload.accounts) ? (payload.accounts as TokenAccountRow[]) : [];

  const usages = rows.map((row) => row?.usage_fraction).filter(isFiniteNumber);
  const resets = rows
    .map((row) => row?.limit_window_reset_at)
    .filter((value): value is string => typeof value === "string" && value.length > 0)
    .sort();

  return {
    account_count: rows.length,
    exhausted_count: rows.filter((row) => row?.exhausted === true).length,
    mean_usage_fraction: usages.length ? round4(usages.reduce((sum, u) => sum + u, 0) / usages.length) : null,
    max_usage_fraction: usages.length ? round4(Math.max(...usages)) : null,
    next_limit_window_reset_at: resets[0] ?? null,
  };
}

/** One entry inside `host.health`'s `managed_repos` roster, as the daemon
 * sends it — always the real slug, regardless of visibility (see
 * `ManagedRepoEntry`'s Rust-side doc: the daemon carries full detail, the
 * redaction boundary is here). */
interface ManagedRepoRow {
  slug?: unknown;
  visibility?: unknown;
}

/**
 * Redact one `host.health.managed_repos` array for a public, unauthenticated
 * viewer (Issue #4976's anti-leak contract): a `public`-visibility entry's
 * slug survives; a `private` entry's slug is dropped but the entry itself is
 * kept (so the roster's total count — and therefore "how many private repos"
 * — is still visible without naming any of them). Mirrors
 * `redactActiveSweep`'s "null the identifying field, keep the entry" strategy
 * for a private sweep.
 *
 * A malformed row (wrong-typed `slug`, or any `visibility` other than the
 * literal string `"public"`) is treated as private — the same fail-safe
 * default every visibility decode in this system uses.
 */
export function redactManagedRepos(rows: readonly ManagedRepoRow[]): { slug?: string; visibility: "public" | "private" }[] {
  return rows.map((row) => {
    const isPublic = row?.visibility === "public" && typeof row.slug === "string";
    return isPublic ? { slug: row.slug as string, visibility: "public" as const } : { visibility: "private" as const };
  });
}

/** One `persistent[]` role-tick failure entry inside `host.health`'s `roles`
 * summary (#5022), as the daemon sends it — `root` is the full absolute
 * filesystem workspace path the failing role ran against. */
interface RoleTickFailureRow {
  root?: unknown;
  role?: unknown;
  failures?: unknown;
  last_at?: unknown;
  detail?: unknown;
}

/** `host.health`'s `roles` role-tick health summary (#5022), as the daemon
 * sends it — the daemon always carries full detail (including each failure's
 * absolute `root`); the redaction boundary is here. */
interface RoleTickHealthRow {
  total?: unknown;
  ok?: unknown;
  persistent?: unknown;
}

/** Truncate an absolute workspace path to its final component — mirrors the
 * daemon's `RoleFailure::label()` (`loom-daemon/src/health.rs`) and the
 * frontend's own `pathBasename` (`dashboard/web/src/format.ts`):
 * `/Users/alice/GitHub/loom` → `"loom"`. An empty / slash-only path degrades
 * to itself rather than `""`. */
function pathBasename(root: string): string {
  const parts = root.split("/").filter((part) => part.length > 0);
  return parts.length > 0 ? parts[parts.length - 1]! : root;
}

/**
 * Redact one `host.health.roles` summary (#5022) for a public, unauthenticated
 * viewer. The counts (`total`/`ok`) and each persistent failure's `role`,
 * `failures`, and `last_at` survive; every entry's `root` — a full absolute
 * filesystem workspace path whose home-directory segment names the *operator*
 * on the common macOS/Linux layout — is truncated to its basename, exactly as
 * the daemon's `RoleFailure::label()` and the frontend's `pathBasename`
 * already render it for display. Only the raw wire payload to the public,
 * unauthenticated API skipped that truncation; this closes it. The
 * authenticated `/api/*` surface keeps the full path (this derivation runs on
 * the public path only).
 *
 * `detail` is deliberately dropped, not truncated (#5065): unlike `root`, it
 * is not "non-path detail" — the daemon's own `RoleTickOutcome::Failure`
 * constructors (`loom-daemon/src/role_runner.rs`) build it by interpolating
 * an absolute `script.display()` path plus up to `MAX_OUTPUT_TAIL_BYTES` of
 * the failing role child's own log tail, which can itself contain further
 * absolute paths, repo slugs, issue numbers, or branch names. A free-form tail
 * of another process's output cannot be shown safe by construction, so —
 * unlike `root` — there is no basename-style truncation that makes it safe;
 * it stays behind the Access gate, same footing as `tokens.snapshot`'s
 * `accounts`. The authenticated `/api/*` surface still returns it unchanged.
 *
 * Like `redactManagedRepos`, a per-field pick (not a spread), so a future
 * field added to the `roles` schema is dropped from the public view by default
 * until this table is deliberately updated — the fail-safe direction never
 * flips silently. A non-string `root` (malformed payload) is dropped rather
 * than copied through, so a raw path can never reach a public response by any
 * type-confusion path.
 */
export function redactRoleTickHealth(roles: RoleTickHealthRow): Record<string, unknown> {
  const redacted: Record<string, unknown> = {};
  if ("total" in roles) redacted.total = roles.total;
  if ("ok" in roles) redacted.ok = roles.ok;
  if (Array.isArray(roles.persistent)) {
    redacted.persistent = (roles.persistent as RoleTickFailureRow[]).map((row) => {
      const entry: Record<string, unknown> = {};
      if (row && typeof row === "object") {
        if (typeof row.root === "string") entry.root = pathBasename(row.root);
        if ("role" in row) entry.role = row.role;
        if ("failures" in row) entry.failures = row.failures;
        if ("last_at" in row) entry.last_at = row.last_at;
        // `detail` is intentionally NOT copied here — see the doc comment.
      }
      return entry;
    });
  }
  return redacted;
}

/**
 * Per-kind *derivations* layered on top of the field allowlist: fields the
 * public view gets that are computed from redacted-away input rather than
 * copied from it.
 *
 * Deliberately a second, separate step from `RECORD_FIELD_ALLOWLIST` so the
 * fail-safe direction still holds: the allowlist alone decides what is
 * *copied*, and deleting an entry here can only ever remove data from a
 * public response, never add it. (Doing this as an allowlist entry plus an
 * in-place rewrite would mean a future edit that skipped the rewrite leaked
 * the raw field.)
 */
const PUBLIC_RECORD_DERIVATIONS: Readonly<Record<string, (payload: Record<string, unknown>) => Record<string, unknown>>> =
  {
    "tokens.snapshot": (payload) => deriveTokenPoolAggregate(payload) as unknown as Record<string, unknown>,
    // `managed_repos` (#4976) and `roles` (#5022) are both deliberately ABSENT
    // from `RECORD_FIELD_ALLOWLIST` — like `tokens.snapshot`'s `accounts`,
    // each has a raw form that can identify a private repo (`managed_repos`)
    // or the operator via a home-directory path (`roles[].root`), so each only
    // ever reaches a public response through this derivation. Each is absent
    // entirely from the output when the payload carries no such field at all
    // (a pre-#4976 / pre-#5022 daemon, or a host with no registered workspaces
    // / no sampled role ticks), so the field-presence contract stays "the
    // daemon sent this" rather than "this module always adds it".
    "host.health": (payload) => {
      const derived: Record<string, unknown> = {};
      if (Array.isArray(payload.managed_repos)) {
        derived.managed_repos = redactManagedRepos(payload.managed_repos as ManagedRepoRow[]);
      }
      if (payload.roles && typeof payload.roles === "object" && !Array.isArray(payload.roles)) {
        derived.roles = redactRoleTickHealth(payload.roles as RoleTickHealthRow);
      }
      return derived;
    },
  };

/** The fail-safe allowlist for a `kind` this table does not (yet) recognize
 * — an unknown, forward-compatible record kind (per the schema doc's
 * `schema_version` contract) reveals only its own `kind` string until this
 * module is updated to classify it deliberately. */
const DEFAULT_ALLOWLIST: readonly string[] = ["kind"];

/** Project `payload` down to the fields `kind`'s allowlist permits. Used for
 * every nested `record` payload this module redacts — history records, live
 * tail SSE frames, and the Durable Object's per-host health/tokens entries
 * alike, so there is exactly one allowlist table for the whole backend. */
export function redactPayload(kind: string, payload: Record<string, unknown>): Record<string, unknown> {
  const allowlist = RECORD_FIELD_ALLOWLIST[kind] ?? DEFAULT_ALLOWLIST;
  const redacted: Record<string, unknown> = {};
  for (const field of allowlist) {
    if (field in payload) {
      redacted[field] = payload[field];
    }
  }
  const derive = PUBLIC_RECORD_DERIVATIONS[kind];
  if (derive) {
    Object.assign(redacted, derive(payload));
  }
  return redacted;
}

function isPrivateAndUnauthenticated(visibility: RepoVisibility, isAuthenticated: boolean): boolean {
  return !isAuthenticated && visibility === "private";
}

// ---------------------------------------------------------------------------
// `GET /api/history` / `GET /public/history`
// ---------------------------------------------------------------------------

/** Redact one `HistoryRecord` for the given auth state. An authenticated
 * viewer or a `public`-visibility record is returned unchanged (same
 * top-level shape query.ts already produces); a private record viewed
 * without authentication has every repo/issue/sweep-identifying field
 * nulled and its nested `record` payload projected through
 * `redactPayload`. */
export function redactHistoryRecord(record: HistoryRecord, isAuthenticated: boolean): HistoryRecord {
  if (!isPrivateAndUnauthenticated(decodeVisibility(record.visibility), isAuthenticated)) {
    return record;
  }
  return {
    id: record.id,
    schemaVersion: record.schemaVersion,
    emittedAt: record.emittedAt,
    hostId: record.hostId,
    kind: record.kind,
    // The three leak vectors AC2 names by field: repo name, issue number,
    // and the sweep id (which embeds the issue number, e.g.
    // "sweep-issue-4703-0" — see `.loom/docs/telemetry-schema.md`).
    repo: null,
    visibility: record.visibility,
    issue: null,
    sweepId: null,
    ingestedAt: record.ingestedAt,
    record: redactPayload(record.kind, record.record),
  };
}

/** Redact every record in a `queryHistory` result, preserving pagination
 * metadata unchanged (the cursor is an opaque row id, not itself
 * identifying). */
export function redactHistoryQueryResult(
  result: HistoryQueryResult,
  isAuthenticated: boolean,
): HistoryQueryResult {
  return {
    records: result.records.map((record) => redactHistoryRecord(record, isAuthenticated)),
    nextCursor: result.nextCursor,
  };
}

// ---------------------------------------------------------------------------
// `GET /api/fleet-state` / `GET /public/fleet-state`
// ---------------------------------------------------------------------------

/** The shape a redacted `ActiveSweepState` is returned as. `sweepId` is
 * `string` (required) on the wire type this backend accepts internally, but
 * a redacted private/public entry omits it entirely (not nulled) —
 * `JSON.stringify` drops an `undefined` property, so the field is simply
 * absent from the response rather than present-but-null. */
export interface PublicActiveSweep {
  hostId: string;
  visibility: "public" | "private";
  repo?: string;
  issue?: number;
  sweepId?: string;
  phase?: string;
  startedAt?: string;
  enteredPhaseAt?: string;
  model?: string;
  effort?: string;
  updatedAt: string;
}

/** Redact one `ActiveSweepState` (the Durable Object's live per-sweep
 * entry). Mirrors `redactHistoryRecord`'s field selection: repo/issue/sweep
 * id are stripped for a private, unauthenticated view; every other field
 * (phase, timing, model/effort) is aggregate/lifecycle metadata, not
 * repo-identifying, and survives. */
export function redactActiveSweep(sweep: ActiveSweepState, isAuthenticated: boolean): PublicActiveSweep {
  if (!isPrivateAndUnauthenticated(sweep.visibility, isAuthenticated)) {
    return sweep;
  }
  return {
    hostId: sweep.hostId,
    visibility: sweep.visibility,
    phase: sweep.phase,
    startedAt: sweep.startedAt,
    enteredPhaseAt: sweep.enteredPhaseAt,
    model: sweep.model,
    effort: sweep.effort,
    updatedAt: sweep.updatedAt,
  };
}

export interface RedactedFleetSnapshot {
  hosts: FleetSnapshot["hosts"];
  activeSweeps: PublicActiveSweep[];
}

/** Redact a full `FleetSnapshot`: every host's `health`/`tokens` entry is
 * projected through `redactPayload`, and every `activeSweeps` entry through
 * `redactActiveSweep`.
 *
 * **An authenticated viewer's host entries are returned untouched.** This
 * used to call `redactPayload` unconditionally, which was harmless only
 * while every allowlist happened to name every field the schema defined. The
 * moment one did not — `tokens.snapshot`, which now summarizes `accounts`
 * for the public view — that would have stripped per-account detail from the
 * authenticated dashboard too. The auth check belongs here, on the same
 * footing as `redactActiveSweep`'s. */
export function redactFleetSnapshot(snapshot: FleetSnapshot, isAuthenticated: boolean): RedactedFleetSnapshot {
  const hosts: FleetSnapshot["hosts"] = {};
  for (const [hostId, entry] of Object.entries(snapshot.hosts)) {
    hosts[hostId] = isAuthenticated
      ? entry
      : {
          ...(entry.health && {
            health: {
              record: redactPayload("host.health", entry.health.record),
              updatedAt: entry.health.updatedAt,
              // Staleness (issue #4957) is derived, non-identifying timing
              // metadata — same footing as `updatedAt` itself, so it passes
              // through the redaction boundary unchanged for both viewers.
              freshness: entry.health.freshness,
            },
          }),
          ...(entry.tokens && {
            tokens: {
              record: redactPayload("tokens.snapshot", entry.tokens.record),
              updatedAt: entry.tokens.updatedAt,
              freshness: entry.tokens.freshness,
            },
          }),
        };
  }
  return {
    hosts,
    activeSweeps: snapshot.activeSweeps.map((sweep) => redactActiveSweep(sweep, isAuthenticated)),
  };
}

// ---------------------------------------------------------------------------
// `GET /api/events` / `GET /public/events` — live tail SSE frames
// ---------------------------------------------------------------------------

const SSE_DATA_PREFIX = "data: ";
const SSE_FRAME_SUFFIX = "\n\n";

interface LiveTailFramePayload {
  topic: string;
  event: {
    hostId: string;
    emittedAt: string;
    schemaVersion: number;
    record: Record<string, unknown>;
  };
}

/**
 * Redact one already-framed SSE chunk from `query.ts`'s `createLiveTailStream`
 * (`src/index.ts` pipes every stream chunk through this before it reaches
 * the client). A non-`data:` frame (the leading `retry:`/comment preamble,
 * or a `: keepalive` comment) carries no record and passes through
 * unchanged. A malformed/unexpected `data:` frame also passes through
 * unchanged rather than throwing — this function must never crash a live
 * connection; `query.ts`'s own frames are always well-formed in practice, so
 * this is a defensive fallback, not the primary control.
 */
export function redactSseFrame(frameText: string, isAuthenticated: boolean): string {
  if (isAuthenticated || !frameText.startsWith(SSE_DATA_PREFIX) || !frameText.endsWith(SSE_FRAME_SUFFIX)) {
    return frameText;
  }

  const jsonText = frameText.slice(SSE_DATA_PREFIX.length, -SSE_FRAME_SUFFIX.length);
  let payload: LiveTailFramePayload;
  try {
    payload = JSON.parse(jsonText) as LiveTailFramePayload;
  } catch {
    return frameText;
  }
  if (!payload || typeof payload !== "object" || !payload.event || typeof payload.event !== "object") {
    return frameText;
  }

  const visibility = decodeVisibility(payload.event.record?.visibility);
  if (visibility === "public") return frameText;

  const redacted: LiveTailFramePayload = {
    topic: payload.topic,
    event: {
      hostId: payload.event.hostId,
      emittedAt: payload.event.emittedAt,
      schemaVersion: payload.event.schemaVersion,
      record: redactPayload(payload.topic, payload.event.record ?? {}),
    },
  };
  return `${SSE_DATA_PREFIX}${JSON.stringify(redacted)}${SSE_FRAME_SUFFIX}`;
}

/** Wrap a live-tail `ReadableStream<Uint8Array>` so every SSE frame is
 * redacted before it reaches the client. Relies on the stream's own framing
 * discipline (`createLiveTailStream` enqueues one complete `data: ...\n\n`
 * — or `retry:`/`: keepalive` — string per chunk, never a partial frame or
 * multiple frames in one chunk), which the Streams spec preserves 1:1
 * through a `TransformStream` (no chunk coalescing on the writable→readable
 * path). */
export function redactLiveTailStream(
  stream: ReadableStream<Uint8Array>,
  isAuthenticated: boolean,
): ReadableStream<Uint8Array> {
  if (isAuthenticated) return stream;

  const decoder = new TextDecoder();
  const encoder = new TextEncoder();
  const transform = new TransformStream<Uint8Array, Uint8Array>({
    transform(chunk, controller) {
      const frameText = decoder.decode(chunk, { stream: true });
      controller.enqueue(encoder.encode(redactSseFrame(frameText, isAuthenticated)));
    },
  });
  return stream.pipeThrough(transform);
}
