/**
 * The dashboard's only data source.
 *
 * **`GET /api/fleet-state`, and nothing else** (Epic #4702's push-model
 * architecture): the UI never opens a connection to a fleet host's
 * `loom-daemon`. This is the substantive difference from the surface it
 * replaces — `loom-daemon serve`'s `--peers` panel
 * (`loom-daemon/src/dashboard.html`'s `refreshPeers`) fans out from the
 * *browser* to every peer's `/api/status`, so it needs each peer to be
 * network-reachable from the viewer and it degrades per-peer. Here the Worker
 * has already aggregated the fleet from pushed telemetry, and one request
 * returns all of it.
 *
 * **Two datasets, one app** (issue #4795's single-URL layout). The Worker
 * serves this bundle at `/` to everyone — signed in or not — and stamps the
 * viewer's auth state into `window.__LOOM_FLEET__` after validating the
 * Cloudflare Access JWT itself (`../../src/index.ts`'s `handleRoot`). This
 * client reads that flag and targets the dataset the viewer is entitled to:
 * `/api/*` when signed in, `/public/*` when not.
 *
 * **The flag is a routing hint, not a permission.** Nothing here — and
 * nothing a browser can do to it — grants access to anything: `/api/*`
 * returns full detail only to a request carrying a valid Access session, and
 * `/public/*` is redacted server-side by `../../src/redaction.ts` regardless
 * of who asks. Flipping the global in a devtools console just points the UI
 * at an endpoint that will refuse it. The enforcement is entirely
 * server-side; this is only about asking for the right thing first.
 *
 * A stale session (signed in when the page loaded, expired since) still
 * yields a 401/403 on `/api/*` — see `AUTH_HINT_STATUSES`.
 */

import { parseFleetSnapshot } from "./parse";
import type { FleetSnapshot } from "./types";

/**
 * Empty = same origin, which is the deployed configuration: the Worker serves
 * both this bundle (Workers Assets) and `/api/*`, so there is no CORS and no
 * cross-origin credential problem. `VITE_API_BASE` exists only for the
 * split-deploy case (UI on Pages, API on a different hostname); see
 * `../README.md`.
 */
export const API_BASE: string = import.meta.env?.VITE_API_BASE ?? "";

/** The authenticated dataset: full detail, including per-account token-pool
 * rows. Requires a valid Access session. */
export const FLEET_STATE_PATH = "/api/fleet-state";

/** The public dataset: private-repo rows reduced to lifecycle/timing, and
 * token-pool state reduced to non-identifying aggregates. */
export const PUBLIC_FLEET_STATE_PATH = "/public/fleet-state";

/** The global `handleRoot` injects into the SPA shell. */
const AUTH_STATE_GLOBAL = "__LOOM_FLEET__";

/** The auth state the Worker stamps into the page. `email` is present only
 * for an authenticated viewer whose token carried one. `commit` (issue
 * #4958) is present whenever the deployment stamps one, for either viewer —
 * unlike `email` it is not identity, so it is not gated on `authenticated`. */
export interface AuthState {
  authenticated: boolean;
  email?: string;
  commit?: string;
}

/**
 * Read the server-injected auth state.
 *
 * Defaults to **anonymous** whenever the global is missing or malformed — a
 * UI that guesses "authenticated" would request `/api/*`, get a 401, and show
 * an error page to a visitor who was entitled to the public view. Guessing
 * "public" merely under-fetches for someone who can reload.
 */
export function readAuthState(scope: typeof globalThis = globalThis): AuthState {
  const raw = (scope as Record<string, unknown>)[AUTH_STATE_GLOBAL];
  if (typeof raw !== "object" || raw === null) return { authenticated: false };

  const state = raw as { authenticated?: unknown; email?: unknown; commit?: unknown };
  const commit = typeof state.commit === "string" && state.commit.length > 0 ? { commit: state.commit } : {};

  if (state.authenticated !== true) return { authenticated: false, ...commit };

  return {
    authenticated: true,
    ...(typeof state.email === "string" && state.email.length > 0 ? { email: state.email } : {}),
    ...commit,
  };
}

/** Whether the viewer is signed in. Thin wrapper over `readAuthState` — the
 * common case, and the one the data-fetching path cares about. */
export function isAuthenticatedViewer(scope: typeof globalThis = globalThis): boolean {
  return readAuthState(scope).authenticated;
}

/** The fleet-state path for a given auth state. */
export function fleetStatePath(authenticated: boolean): string {
  return authenticated ? FLEET_STATE_PATH : PUBLIC_FLEET_STATE_PATH;
}

/** Statuses that mean "your Access session, not the backend" — worth a
 * different remedy in the error state than a generic failure. */
const AUTH_HINT_STATUSES = new Set([401, 403]);

export class FleetStateError extends Error {
  readonly status?: number;
  /** True when the failure looks like an expired/missing Cloudflare Access
   * session rather than a backend fault, so the UI can suggest reloading to
   * re-authenticate instead of "try again later". */
  readonly isAuthHint: boolean;

  constructor(message: string, options: { status?: number; cause?: unknown } = {}) {
    super(message, options.cause === undefined ? undefined : { cause: options.cause });
    this.name = "FleetStateError";
    this.status = options.status;
    this.isAuthHint = options.status !== undefined && AUTH_HINT_STATUSES.has(options.status);
  }
}

export interface FetchFleetStateOptions {
  /** Injected in tests; defaults to the global `fetch`. */
  fetchImpl?: typeof fetch;
  signal?: AbortSignal;
  baseUrl?: string;
  /** Override the dataset choice. Defaults to the server-injected auth state
   * (`isAuthenticatedViewer`); tests pin it explicitly. */
  authenticated?: boolean;
}

/**
 * Fetch and narrow the current fleet snapshot.
 *
 * Throws `FleetStateError` on transport failure, a non-2xx status, or a body
 * that is not JSON. A *structurally odd but parseable* body is not an error —
 * `parseFleetSnapshot` degrades it (see that module's doc), because a single
 * malformed host entry should cost that host's card, not the whole page.
 */
export async function fetchFleetState(options: FetchFleetStateOptions = {}): Promise<FleetSnapshot> {
  const fetchImpl = options.fetchImpl ?? globalThis.fetch;
  const path = fleetStatePath(options.authenticated ?? isAuthenticatedViewer());
  const url = `${options.baseUrl ?? API_BASE}${path}`;

  let response: Response;
  try {
    response = await fetchImpl(url, {
      headers: { accept: "application/json" },
      // Access issues a session cookie; without this an authenticated deploy
      // would 302 every poll.
      credentials: "same-origin",
      ...(options.signal ? { signal: options.signal } : {}),
    });
  } catch (cause) {
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    throw new FleetStateError(`could not reach ${url}: ${errorMessage(cause)}`, { cause });
  }

  if (!response.ok) {
    throw new FleetStateError(
      AUTH_HINT_STATUSES.has(response.status)
        ? `${response.status} from ${path} — your Cloudflare Access session may have expired`
        : `${path} returned HTTP ${response.status}`,
      { status: response.status },
    );
  }

  let body: unknown;
  try {
    body = await response.json();
  } catch (cause) {
    throw new FleetStateError(`${path} did not return JSON`, {
      status: response.status,
      cause,
    });
  }

  return parseFleetSnapshot(body);
}

function errorMessage(value: unknown): string {
  return value instanceof Error ? value.message : String(value);
}
