/**
 * The dashboard HTML page (Epic #4702, Phase 3, issue #4753), rendered by
 * `src/index.ts`'s root `/` handler in one of two variants (issue #4795 made
 * `/` single-URL: the same route serves both, chosen by whether
 * `accessAuth.ts` validated a visitor's Access JWT):
 *
 * - **Public** (`options.isAuthenticated` falsy, the default): sourced from
 *   the exact same redacted data `GET /public/fleet-state` and
 *   `GET /public/history` return — the caller calls
 *   `redactFleetSnapshot`/`redactHistoryQueryResult` directly, in-process
 *   (not over HTTP), before handing data to this module, so there is no code
 *   path by which this variant could accidentally source data from the
 *   authenticated `/api/*` surface. Its inline script tails
 *   `GET /public/events` for live updates.
 * - **Authenticated** (`options.isAuthenticated: true`): the caller passes
 *   the raw, unredacted snapshot/history straight through instead — see
 *   `renderPublicPage`'s own doc for the exact contract. Its inline script
 *   tails `GET /api/events` (full detail) instead.
 *
 * See `./redaction.ts`'s module doc for the redaction policy the public
 * variant renders faithfully rather than reimplements: this module MUST NOT
 * re-derive private/public status or attempt to reconstruct a stripped
 * field — it only ever formats fields that are already present on the
 * objects it is handed.
 *
 * ## Display allowlist: narrower than the wire allowlist, on purpose
 *
 * `PUBLIC_PAGE_DISPLAY_FIELDS` below decides which record `kind`s and which
 * of their fields this page renders at all. It is deliberately a *subset* of
 * `redaction.ts`'s `RECORD_FIELD_ALLOWLIST` — most notably, it omits
 * `tokens.snapshot` entirely (see the next section) — and, like that module,
 * is itself an allowlist (a kind/field not listed is never rendered), so a
 * future wire field never leaks into the page just because a maintainer
 * forgot to update this table.
 *
 * ## Token/cost widget decision (resolves the open question in issue #4752)
 *
 * `tokens.snapshot` passes through `GET /public/fleet-state` and
 * `GET /public/history` unredacted (see `redaction.ts`'s module doc) because
 * its wire allowlist has no `repo` field to key the private/public split
 * off. But its `accounts[].account` field is a per-account identifier (an
 * operator's Claude account label) that is not one of the four leak vectors
 * this epic's redaction guarantees cover (repo/issue/branch/PR) and is
 * reasonably sensitive on its own — the exact concern issue #4752 flags and
 * asks this issue to resolve. Decision: this page never renders
 * `tokens.snapshot` — not in the fleet overview, not in history, not in the
 * live feed — regardless of what the `/public/*` API technically returns.
 * `PUBLIC_PAGE_DISPLAY_FIELDS` has no entry for it, so it is filtered out by
 * construction rather than by a special-cased check; widening this later
 * (e.g. an aggregate, non-identifying burn-rate widget) is a decision for
 * whatever implements #4752, made explicit rather than defaulted into.
 *
 * ## Read-only
 *
 * This page contains no `<form>`, no mutating `fetch`/`EventSource` call,
 * and no reference to `/admin/*` or `/ingest` — the public variant only ever
 * reads `/public/*`; the authenticated variant only ever reads `/api/*`.
 */

import { classifyFreshness, type FreshnessInfo, type HostFreshness } from "./fleetState";
import type { HistoryQueryResult, HistoryRecord } from "./query";
import type { PublicActiveSweep, RedactedFleetSnapshot } from "./redaction";

/** The record `kind`s this page renders, and — for each — the nested
 * `record` payload fields it displays. A `kind` absent from this table
 * (including `tokens.snapshot` — see the module doc) is never rendered by
 * `renderHistoryTable` or the live-feed script, no matter what the
 * `/public/*` API returns for it. */
const PUBLIC_PAGE_DISPLAY_FIELDS: Readonly<Record<string, readonly string[]>> = {
  "sweep.started": ["started_at", "model", "effort"],
  "sweep.phase": ["phase", "entered_at"],
  "sweep.completed": ["completed_at", "result"],
  "sweep.outcome": ["model", "effort", "result", "total_duration_sec"],
  // `build_commit`/`built_at` (#4956) sit next to `daemon_version` because
  // the version alone only moves once per release — the commit is what makes
  // two same-version builds distinguishable. Both are non-identifying build
  // metadata about the binary, not about any repo or operator, so they are as
  // public as the version string itself.
  "host.health": [
    "captured_at",
    "daemon_version",
    "build_commit",
    "built_at",
    "uptime_sec",
    "logical_cpus",
    "cpu_idle_fraction",
  ],
};

export function isPublicPageDisplayKind(kind: string): boolean {
  return Object.prototype.hasOwnProperty.call(PUBLIC_PAGE_DISPLAY_FIELDS, kind);
}

function escapeHtml(value: unknown): string {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function formatFieldValue(value: unknown): string {
  return typeof value === "object" && value !== null ? JSON.stringify(value) : String(value);
}

/** Render the `kind`-specific detail fields for one record's payload, per
 * `PUBLIC_PAGE_DISPLAY_FIELDS` — a `kind` with no entry renders no detail
 * (callers are expected to have already filtered it out via
 * `isPublicPageDisplayKind`). */
function formatDetails(kind: string, record: Record<string, unknown>): string {
  const fields = PUBLIC_PAGE_DISPLAY_FIELDS[kind] ?? [];
  const parts: string[] = [];
  for (const field of fields) {
    const value = record[field];
    if (value !== undefined) {
      parts.push(`${escapeHtml(field)}=${escapeHtml(formatFieldValue(value))}`);
    }
  }
  return parts.join(", ");
}

/** Render a "repo #issue (sweepId)" cell, or a "private" placeholder when
 * `repo` is absent/null — matching the already-redacted absence of those
 * fields rather than re-deciding visibility itself. */
function renderRepoCell(repo: string | null | undefined, issue: number | null | undefined, sweepId?: string | null): string {
  if (!repo) return `<span class="muted">private</span>`;
  const issuePart = issue ? ` #${escapeHtml(issue)}` : "";
  const sweepPart = sweepId ? ` <span class="muted">(${escapeHtml(sweepId)})</span>` : "";
  return `${escapeHtml(repo)}${issuePart}${sweepPart}`;
}

// ---------------------------------------------------------------------------
// Fleet overview (`/public/fleet-state`)
// ---------------------------------------------------------------------------

function renderActiveSweepRow(sweep: PublicActiveSweep): string {
  return `<tr>
    <td>${escapeHtml(sweep.hostId)}</td>
    <td>${escapeHtml(sweep.visibility)}</td>
    <td>${renderRepoCell(sweep.repo, sweep.issue, sweep.sweepId)}</td>
    <td>${escapeHtml(sweep.phase ?? "")}</td>
    <td>${escapeHtml(sweep.model ?? "")}</td>
    <td>${escapeHtml(sweep.effort ?? "")}</td>
    <td>${escapeHtml(sweep.startedAt ?? "")}</td>
    <td>${escapeHtml(sweep.updatedAt)}</td>
  </tr>`;
}

// ---------------------------------------------------------------------------
// Host staleness (issue #4957): LIVE/STALE/OFFLINE, not a raw timestamp
// forever presented as current — see `fleetState.ts`'s `classifyFreshness`
// for the shared cadence/boundary policy this page renders faithfully
// rather than reimplements.
// ---------------------------------------------------------------------------

const FRESHNESS_LABEL: Readonly<Record<HostFreshness, string>> = {
  live: "LIVE",
  stale: "STALE",
  offline: "OFFLINE",
};

/** "just now" / "5m ago" / "3h ago" / "2d ago" — coarse enough to read at a
 * glance, exact enough to distinguish "missed one sample" from "gone for
 * days"; the full ISO timestamp is always still available in the cell's
 * `title` attribute for anyone who wants precision. */
function formatAge(ageSeconds: number): string {
  if (!Number.isFinite(ageSeconds)) return "unknown";
  if (ageSeconds < 60) return "just now";
  const minutes = Math.floor(ageSeconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/** The freshness badge + "last seen …" qualifier — rendered directly beside
 * every cell of host detail so a stale sample's numbers are never shown
 * without it (issue #4957's AC: "its stale metrics are never displayed as
 * current"). */
function renderFreshnessBadge(freshness: FreshnessInfo, updatedAt: string): string {
  return (
    `<span class="freshness-badge freshness-badge--${freshness.status}">${FRESHNESS_LABEL[freshness.status]}</span> ` +
    `<span class="muted" title="${escapeHtml(updatedAt)}">last seen ${escapeHtml(formatAge(freshness.ageSeconds))}</span>`
  );
}

function renderHostHealthRow(
  hostId: string,
  health: { record: Record<string, unknown>; updatedAt: string } | undefined,
  now: Date,
): string {
  if (!health) return "";
  const freshness = classifyFreshness(health.updatedAt, now);
  return `<tr class="freshness-${freshness.status}">
    <td>${escapeHtml(hostId)}</td>
    <td>${renderFreshnessBadge(freshness, health.updatedAt)}</td>
    <td>${formatDetails("host.health", health.record)}</td>
  </tr>`;
}

/** "3 hosts: 2 live, 1 offline (robb-pro, last seen 5h ago)" — the overview
 * heading's explicit totals (issue #4957's AC). Hosts with no `health` entry
 * at all (known only from `activeSweeps`) are counted in `hostIds.length`
 * but contribute no freshness bucket — nothing to classify. */
function renderFreshnessSummary(hostIds: readonly string[], hosts: RedactedFleetSnapshot["hosts"], now: Date): string {
  const counts: Record<HostFreshness, number> = { live: 0, stale: 0, offline: 0 };
  const offlineDescriptions: string[] = [];
  for (const hostId of hostIds) {
    const health = hosts[hostId]?.health;
    if (!health) continue;
    const freshness = classifyFreshness(health.updatedAt, now);
    counts[freshness.status] += 1;
    if (freshness.status === "offline") {
      offlineDescriptions.push(`${hostId}, last seen ${formatAge(freshness.ageSeconds)}`);
    }
  }
  const reporting = counts.live + counts.stale + counts.offline;
  if (reporting === 0) return "";

  const parts = [`${counts.live} live`];
  if (counts.stale > 0) parts.push(`${counts.stale} stale`);
  if (counts.offline > 0) parts.push(`${counts.offline} offline (${offlineDescriptions.join("; ")})`);
  return `: ${parts.join(", ")}`;
}

export function renderFleetOverview(snapshot: RedactedFleetSnapshot, now: Date = new Date()): string {
  const hostIds = Object.keys(snapshot.hosts).sort();
  const healthRows = hostIds
    .map((id) => renderHostHealthRow(id, snapshot.hosts[id]?.health, now))
    .filter((row) => row.length > 0)
    .join("\n");
  const sweepRows = snapshot.activeSweeps.map(renderActiveSweepRow).join("\n");
  const freshnessSummary = renderFreshnessSummary(hostIds, snapshot.hosts, now);

  return `<section id="fleet-overview">
    <h2>Fleet overview</h2>
    <h3>Hosts (${hostIds.length})${escapeHtml(freshnessSummary)}</h3>
    <table>
      <thead><tr><th>Host</th><th>Status</th><th>Detail</th></tr></thead>
      <tbody>${healthRows || `<tr><td colspan="3" class="muted">No host health reported yet.</td></tr>`}</tbody>
    </table>
    <h3>Active sweeps (${snapshot.activeSweeps.length})</h3>
    <table>
      <thead>
        <tr><th>Host</th><th>Visibility</th><th>Repo / Issue</th><th>Phase</th><th>Model</th><th>Effort</th><th>Started</th><th>Updated</th></tr>
      </thead>
      <tbody>${sweepRows || `<tr><td colspan="8" class="muted">No sweeps currently running.</td></tr>`}</tbody>
    </table>
  </section>`;
}

// ---------------------------------------------------------------------------
// History (`/public/history`)
// ---------------------------------------------------------------------------

function renderHistoryRow(record: HistoryRecord): string {
  return `<tr>
    <td>${escapeHtml(record.emittedAt)}</td>
    <td>${escapeHtml(record.hostId)}</td>
    <td>${escapeHtml(record.kind)}</td>
    <td>${renderRepoCell(record.repo, record.issue, record.sweepId)}</td>
    <td>${escapeHtml(record.visibility)}</td>
    <td>${formatDetails(record.kind, record.record)}</td>
  </tr>`;
}

export function renderHistoryTable(history: HistoryQueryResult): string {
  const visible = history.records.filter((record) => isPublicPageDisplayKind(record.kind));
  const rows = visible.map(renderHistoryRow).join("\n");

  return `<section id="history">
    <h2>Recent history</h2>
    <table>
      <thead><tr><th>Emitted</th><th>Host</th><th>Kind</th><th>Repo / Issue</th><th>Visibility</th><th>Detail</th></tr></thead>
      <tbody>${rows || `<tr><td colspan="6" class="muted">No history yet.</td></tr>`}</tbody>
    </table>
  </section>`;
}

// ---------------------------------------------------------------------------
// Live feed (`/public/events`) — client-side only, vanilla JS
// ---------------------------------------------------------------------------

/** The live-feed script's own copy of the display allowlist, injected
 * verbatim from `PUBLIC_PAGE_DISPLAY_FIELDS` at render time so there is a
 * single source of truth for "which kinds/fields does this page show" —
 * the client never has to keep a hand-written allowlist in sync with this
 * module's. A `topic` (== record `kind`) missing from this object is
 * dropped by the `onmessage` handler before touching the DOM. */
function renderLiveFeedScript(eventsPath: string): string {
  const displayFields = JSON.stringify(PUBLIC_PAGE_DISPLAY_FIELDS);
  return `<script>
    (function () {
      var DISPLAY_FIELDS = ${displayFields};
      var MAX_ROWS = 50;
      var feed = document.getElementById("live-feed-body");

      function escapeHtml(value) {
        return String(value == null ? "" : value)
          .replace(/&/g, "&amp;")
          .replace(/</g, "&lt;")
          .replace(/>/g, "&gt;")
          .replace(/"/g, "&quot;")
          .replace(/'/g, "&#39;");
      }

      function formatFieldValue(value) {
        return typeof value === "object" && value !== null ? JSON.stringify(value) : String(value);
      }

      function formatDetails(kind, record) {
        var fields = DISPLAY_FIELDS[kind] || [];
        var parts = [];
        for (var i = 0; i < fields.length; i++) {
          var field = fields[i];
          if (record[field] !== undefined) {
            parts.push(escapeHtml(field) + "=" + escapeHtml(formatFieldValue(record[field])));
          }
        }
        return parts.join(", ");
      }

      function repoCell(record) {
        if (!record.repo) return '<span class="muted">private</span>';
        var issuePart = record.issue ? " #" + escapeHtml(record.issue) : "";
        return escapeHtml(record.repo) + issuePart;
      }

      if (!feed || typeof EventSource === "undefined") return;

      // Read-only: this is the ONLY network call the live feed makes — a GET
      // against the live-tail route matching this page's own auth state
      // (never a mutation).
      var source = new EventSource(${JSON.stringify(eventsPath)});
      source.onmessage = function (evt) {
        var payload;
        try {
          payload = JSON.parse(evt.data);
        } catch (err) {
          return;
        }
        var topic = payload && payload.topic;
        if (!topic || !Object.prototype.hasOwnProperty.call(DISPLAY_FIELDS, topic)) return; // e.g. tokens.snapshot — never rendered
        var event = payload.event || {};
        var record = event.record || {};

        var row = document.createElement("tr");
        row.innerHTML =
          "<td>" + escapeHtml(event.emittedAt) + "</td>" +
          "<td>" + escapeHtml(event.hostId) + "</td>" +
          "<td>" + escapeHtml(topic) + "</td>" +
          "<td>" + repoCell(record) + "</td>" +
          "<td>" + formatDetails(topic, record) + "</td>";
        feed.insertBefore(row, feed.firstChild);
        while (feed.children.length > MAX_ROWS) {
          feed.removeChild(feed.lastChild);
        }
      };
    })();
  </script>`;
}

// ---------------------------------------------------------------------------
// Full page
// ---------------------------------------------------------------------------

const PAGE_STYLE = `
  body { font-family: system-ui, sans-serif; margin: 2rem; color: #1a1a1a; background: #fafafa; }
  h1 { margin-bottom: 0.25rem; }
  .subtitle { color: #555; margin-top: 0; }
  table { border-collapse: collapse; width: 100%; margin-bottom: 1.5rem; background: #fff; }
  th, td { border: 1px solid #ddd; padding: 0.4rem 0.6rem; text-align: left; font-size: 0.9rem; }
  th { background: #f0f0f0; }
  .muted { color: #999; font-style: italic; }
  section { margin-bottom: 2.5rem; }
  .freshness-badge { display: inline-block; padding: 0.1rem 0.4rem; border-radius: 0.25rem; font-size: 0.75rem; font-weight: 600; }
  .freshness-badge--live { background: #d7f5dd; color: #1a7431; }
  .freshness-badge--stale { background: #fff1cc; color: #8a6100; }
  .freshness-badge--offline { background: #f3d4d4; color: #8a1f1f; }
  tr.freshness-stale td, tr.freshness-offline td { color: #888; }
`;

/** Options for {@link renderPublicPage} distinguishing the two callers that
 * share this rendering: the always-public `/public` history (redirected
 * from, pre-#4795) and the single-URL dashboard root `/` (issue #4795),
 * which renders this same page in either its public or authenticated
 * variant depending on whether `accessAuth.ts` validated a JWT. */
export interface RenderPageOptions {
  /** `true` once the caller has already verified the visitor's Access JWT
   * (`src/accessAuth.ts`) — changes only the heading/subtitle copy below;
   * it does NOT change what this function renders. The caller is entirely
   * responsible for choosing redacted vs. unredacted `snapshot`/`history`
   * to match this flag (see the function doc). */
  isAuthenticated?: boolean;
  /** Where the "Sign in" link (shown only when `isAuthenticated` is falsy)
   * points. Defaults to `/login` (issue #4795's new Access-gated route). */
  loginPath?: string;
}

/**
 * Render the full dashboard HTML document — the public, redacted view when
 * `options.isAuthenticated` is falsy/absent, or the full authenticated view
 * when it is `true`. This function performs no redaction itself, only
 * formatting, per the module doc's single-enforcement-point rule
 * (`redaction.ts` owns redaction; this module owns display) — **callers are
 * entirely responsible for the data/flag pairing**:
 *
 * - Public (`isAuthenticated` falsy): `snapshot`/`history` MUST already have
 *   passed through `redactFleetSnapshot`/`redactHistoryQueryResult` with
 *   `isAuthenticated: false`.
 * - Authenticated (`isAuthenticated: true`): pass the raw, unredacted
 *   snapshot/history straight from `fleetStateStub`/`queryHistory` — do NOT
 *   redact first, or an authenticated operator would see the public view's
 *   data despite the "Dashboard" heading.
 */
export function renderPublicPage(
  snapshot: RedactedFleetSnapshot,
  history: HistoryQueryResult,
  options: RenderPageOptions = {},
): string {
  const isAuthenticated = options.isAuthenticated ?? false;
  const loginPath = options.loginPath ?? "/login";

  const heading = isAuthenticated ? "Loom Fleet — Dashboard" : "Loom Fleet — Public View";
  const subtitle = isAuthenticated
    ? `<p class="subtitle">Signed in — full, unredacted fleet detail.</p>`
    : `<p class="subtitle">
    Unauthenticated, redacted snapshot of fleet activity. Private-repository
    records show only aggregate lifecycle/timing detail — never a repo name,
    issue, branch, or PR link.
    <a href="${escapeHtml(loginPath)}">Sign in</a> for the full view.
  </p>`;

  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>${heading}</title>
  <style>${PAGE_STYLE}</style>
</head>
<body>
  <h1>${heading}</h1>
  ${subtitle}

  ${renderFleetOverview(snapshot)}
  ${renderHistoryTable(history)}

  <section id="live-feed">
    <h2>Live feed</h2>
    <table>
      <thead><tr><th>Emitted</th><th>Host</th><th>Kind</th><th>Repo / Issue</th><th>Detail</th></tr></thead>
      <tbody id="live-feed-body"><tr><td colspan="5" class="muted">Waiting for events&hellip;</td></tr></tbody>
    </table>
  </section>

  ${renderLiveFeedScript(isAuthenticated ? "/api/events" : "/public/events")}
</body>
</html>`;
}
