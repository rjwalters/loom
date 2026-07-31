/**
 * The token/cost analytics panel: burn curves, limit-window forecasts, and
 * per-repo attribution (Epic #4702, Phase 3, issue #4752).
 *
 * ## Public-exposure decision: authenticated surface only
 *
 * Phase 2's redaction policy (`../../src/redaction.ts`) passes
 * `tokens.snapshot` through **unredacted on both `/api` and `/public`** — the
 * kind carries no `repo`, so the private/public visibility split has nothing
 * to key on. That is a correct backend decision and this issue does not change
 * it. It is nonetheless *not* a decision that these widgets belong on the
 * public page, and this module resolves that question the other way:
 *
 * **`mountTokenAnalytics` refuses to render on a `"public"` surface.** Three
 * reasons, in descending order of force:
 *
 * 1. **Per-repo attribution is a repo-name table by construction.** The whole
 *    output of `attribution.ts` is "which repositories consumed the fleet's
 *    quota", and repo names are precisely what Phase 2 redacts from
 *    `/public/history` for private-visibility sweeps. Rendering this panel
 *    publicly would reconstruct, by inference from timing, the exact fact the
 *    redaction layer removes.
 * 2. **Account identifiers are operator infrastructure.** `agent-3` +
 *    `usage_fraction` + `limit_window_reset_at` is a live capacity map of the
 *    operator's account pool: it says how many accounts exist, which are near
 *    their cap, and when each recovers. That is useful to an operator and
 *    useful to nobody else in a way that benefits the operator.
 * 3. **Exhaustion forecasts are a scheduling signal.** "This fleet runs dry in
 *    40 minutes" is exactly the sort of operational state a public status page
 *    should be a deliberate choice to publish, not a side effect of which API
 *    route happens to permit it.
 *
 * This is a **UI-layer** decision, enforced here and (independently) by
 * `api.ts` pinning its fetch to `/api`. It changes no redaction behavior. If a
 * future operator wants a public capacity summary, the right shape is a
 * purpose-built aggregate (e.g. "fleet capacity: healthy") with no account or
 * repo names — a new component, not a flag on this one. Coordinated with the
 * public view page (#4753) via `../../docs/token-analytics.md`.
 */

import type { AccountBurnCurve, BurnSegment } from "./burn.js";
import { buildBurnCurves } from "./burn.js";
import type { AccountForecast, ForecastStatus } from "./forecast.js";
import { forecastAccounts } from "./forecast.js";
import type { AttributionResult } from "./attribution.js";
import { attributeUsageToRepos } from "./attribution.js";
import { parseSweepWindows, parseTokenSamples } from "./parse.js";
import type { HistoryEnvelope, SweepWindow, TokenSample } from "./types.js";
import { fetchHistory } from "./api.js";
import { formatDuration, formatInstant, formatPercent, formatRatePerHour, formatRelative, UNKNOWN } from "./format.js";

/** Which route surface the panel is being rendered on. */
export type DashboardSurface = "authenticated" | "public";

/** The only surface this panel renders on — see the module doc. */
export const REQUIRED_SURFACE: DashboardSurface = "authenticated";

export interface TokenAnalytics {
  curves: AccountBurnCurve[];
  forecasts: AccountForecast[];
  attribution: AttributionResult;
  samples: TokenSample[];
  sweeps: SweepWindow[];
}

export interface ComputeOptions {
  /** Reference instant (epoch ms); injectable for deterministic tests. */
  now?: number;
}

/** Partition a history page into the two record families and run all three
 * analytics over them. Pure — no DOM, no network — so the whole computation is
 * unit-testable from fixtures. */
export function computeTokenAnalytics(
  records: readonly HistoryEnvelope[],
  options: ComputeOptions = {},
): TokenAnalytics {
  const samples = parseTokenSamples(records);
  const sweeps = parseSweepWindows(records);
  const curves = buildBurnCurves(samples);
  return {
    curves,
    forecasts: forecastAccounts(curves, { now: options.now }),
    attribution: attributeUsageToRepos(samples, sweeps, { now: options.now }),
    samples,
    sweeps,
  };
}

// ---------------------------------------------------------------------------
// DOM helpers — textContent only, never innerHTML
// ---------------------------------------------------------------------------

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  // Repo names, account names and model strings all originate from remote
  // telemetry. They are written as text nodes, never parsed as markup.
  if (text !== undefined) node.textContent = text;
  return node;
}

function cell(row: HTMLTableRowElement, text: string, className?: string): HTMLTableCellElement {
  const td = el("td", className, text);
  row.appendChild(td);
  return td;
}

function headerRow(labels: readonly string[]): HTMLTableSectionElement {
  const thead = document.createElement("thead");
  const tr = document.createElement("tr");
  for (const label of labels) tr.appendChild(el("th", undefined, label));
  thead.appendChild(tr);
  return thead;
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

export interface RenderOptions extends ComputeOptions {
  surface?: DashboardSurface;
}

/**
 * Render the panel into `container` (replacing its contents).
 *
 * Returns `false` without rendering any analytics when the surface is not the
 * authenticated one — the enforcement point for this issue's public-exposure
 * decision (see the module doc). The container instead gets a short notice, so
 * a public page shows an intentional "withheld" state rather than an empty
 * hole that looks like a bug.
 */
export function renderTokenAnalytics(
  container: HTMLElement,
  analytics: TokenAnalytics,
  options: RenderOptions = {},
): boolean {
  const surface = options.surface ?? REQUIRED_SURFACE;
  container.replaceChildren();

  if (surface !== REQUIRED_SURFACE) {
    container.appendChild(renderWithheldNotice());
    return false;
  }

  const now = options.now ?? Date.now();
  const section = el("section", "analytics");
  section.appendChild(el("h2", "analytics__title", "Token & cost analytics"));
  section.appendChild(renderBurnCurves(analytics.curves));
  section.appendChild(renderForecasts(analytics.forecasts, now));
  section.appendChild(renderAttribution(analytics.attribution));
  container.appendChild(section);
  return true;
}

function renderWithheldNotice(): HTMLElement {
  const notice = el("section", "analytics analytics--withheld");
  notice.setAttribute("data-testid", "analytics-withheld");
  notice.appendChild(el("h2", "analytics__title", "Token & cost analytics"));
  notice.appendChild(
    el(
      "p",
      "analytics__note",
      "Per-account detail is operator-only: account identifiers, per-repo attribution and " +
        "per-account exhaustion forecasts are not shown in the public view. Fleet-level " +
        "pool load (accounts in use, how many are exhausted, peak usage) is on the host " +
        "cards above. Sign in for the full breakdown.",
    ),
  );
  return notice;
}

// --- Burn curves -----------------------------------------------------------

function renderBurnCurves(curves: readonly AccountBurnCurve[]): HTMLElement {
  const block = el("div", "analytics__block");
  block.setAttribute("data-testid", "burn-curves");
  block.appendChild(el("h3", "analytics__heading", "Per-account burn curves"));

  if (curves.length === 0) {
    block.appendChild(el("p", "analytics__note", "No tokens.snapshot history in range."));
    return block;
  }

  const list = el("div", "burn-grid");
  for (const curve of curves) list.appendChild(renderBurnCard(curve));
  block.appendChild(list);
  return block;
}

function renderBurnCard(curve: AccountBurnCurve): HTMLElement {
  const exhausted = curve.exhausted;
  const card = el("article", `burn-card${exhausted ? " burn-card--exhausted" : ""}`);
  card.setAttribute("data-account", curve.account);
  card.setAttribute("data-host", curve.hostId);
  card.setAttribute("data-exhausted", String(exhausted));

  const head = el("header", "burn-card__head");
  head.appendChild(el("span", "burn-card__account", curve.account));
  head.appendChild(el("span", "burn-card__host", curve.hostId));
  if (exhausted) {
    // The distinct visual flag AC4 requires: a badge, a card modifier class,
    // and a machine-readable data attribute — belt, braces, and a test hook.
    const badge = el("span", "badge badge--exhausted", "EXHAUSTED");
    badge.setAttribute("data-testid", "exhausted-badge");
    badge.setAttribute("role", "status");
    head.appendChild(badge);
  } else if (curve.everExhausted) {
    head.appendChild(el("span", "badge badge--recovered", "recovered"));
  }
  card.appendChild(head);

  card.appendChild(renderSparkline(curve));

  const latest = curve.points[curve.points.length - 1];
  const foot = el("footer", "burn-card__foot");
  foot.appendChild(el("span", "burn-card__usage", formatPercent(latest?.usageFraction)));
  foot.appendChild(el("span", "burn-card__at", formatInstant(curve.latestAt || undefined)));
  card.appendChild(foot);
  return card;
}

const SPARK_WIDTH = 240;
const SPARK_HEIGHT = 48;
const SVG_NS = "http://www.w3.org/2000/svg";

/**
 * An inline SVG sparkline of `usage_fraction` over time.
 *
 * The y-axis is pinned to `[0, 1]` (not auto-scaled to the data): the whole
 * point of a burn curve is distance from the cap, and an auto-scaled axis makes
 * a 2%-to-4% wobble look identical to a 40%-to-80% sprint. Each burn segment
 * is drawn as its own polyline so a limit-window rollover shows as a break, not
 * as a diagonal plunge that never happened.
 */
function renderSparkline(curve: AccountBurnCurve): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "sparkline");
  svg.setAttribute("viewBox", `0 0 ${SPARK_WIDTH} ${SPARK_HEIGHT}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute(
    "aria-label",
    `${curve.account} usage over time, latest ${formatPercent(curve.points[curve.points.length - 1]?.usageFraction)}`,
  );

  const first = curve.points[0];
  const last = curve.points[curve.points.length - 1];
  if (!first || !last) return svg;

  const span = Math.max(last.at - first.at, 1);
  const x = (at: number): number => ((at - first.at) / span) * SPARK_WIDTH;
  const y = (usage: number): number => SPARK_HEIGHT - usage * SPARK_HEIGHT;

  for (const segment of curve.segments) {
    svg.appendChild(renderSegmentLine(segment, x, y));
  }
  return svg;
}

function renderSegmentLine(
  segment: BurnSegment,
  x: (at: number) => number,
  y: (usage: number) => number,
): SVGPolylineElement {
  const line = document.createElementNS(SVG_NS, "polyline");
  line.setAttribute("class", "sparkline__line");
  // A one-point segment has no line to draw; duplicating the point renders a
  // visible dot instead of silently vanishing.
  const only = segment.points[0];
  const points = segment.points.length === 1 && only ? [only, only] : segment.points;
  line.setAttribute(
    "points",
    points.map((point) => `${x(point.at).toFixed(2)},${y(point.usageFraction).toFixed(2)}`).join(" "),
  );
  return line;
}

// --- Forecasts -------------------------------------------------------------

const STATUS_LABEL: Readonly<Record<ForecastStatus, string>> = {
  exhausted: "Exhausted",
  "projected-exhaustion": "Will exhaust",
  "resets-first": "Resets first",
  flat: "Idle",
  "insufficient-data": "No data",
};

function renderForecasts(forecasts: readonly AccountForecast[], now: number): HTMLElement {
  const block = el("div", "analytics__block");
  block.setAttribute("data-testid", "forecasts");
  block.appendChild(el("h3", "analytics__heading", "Limit-window forecast"));

  if (forecasts.length === 0) {
    block.appendChild(el("p", "analytics__note", "No accounts observed in range."));
    return block;
  }

  const table = el("table", "table table--forecast");
  table.appendChild(
    headerRow(["Account", "Host", "Burn rate", "Usage", "Exhausts", "Window resets", "Margin", "Status"]),
  );
  const tbody = document.createElement("tbody");

  for (const forecast of forecasts) {
    const row = document.createElement("tr");
    row.setAttribute("data-account", forecast.account);
    row.setAttribute("data-status", forecast.status);
    if (forecast.status === "exhausted" || forecast.status === "projected-exhaustion") {
      row.className = "row--at-risk";
    }
    cell(row, forecast.account, "cell--account");
    cell(row, forecast.hostId, "cell--host");
    cell(row, formatRatePerHour(forecast.slopePerHour));
    cell(row, formatPercent(forecast.latestUsageFraction));
    cell(
      row,
      forecast.projectedExhaustionAt === undefined
        ? UNKNOWN
        : `${formatRelative(forecast.secondsUntilExhaustion)} (${formatInstant(forecast.projectedExhaustionAt)})`,
    );
    cell(
      row,
      forecast.limitWindowResetAt === undefined
        ? UNKNOWN
        : `${formatRelative(forecast.secondsUntilReset)} (${formatInstant(forecast.limitWindowResetAt)})`,
    );
    cell(
      row,
      forecast.marginSec === undefined
        ? UNKNOWN
        : forecast.marginSec >= 0
          ? `+${formatDuration(forecast.marginSec)}`
          : `-${formatDuration(forecast.marginSec)}`,
      forecast.marginSec !== undefined && forecast.marginSec < 0 ? "cell--negative" : undefined,
    );
    const status = cell(row, STATUS_LABEL[forecast.status], `status status--${forecast.status}`);
    status.setAttribute("data-testid", `status-${forecast.account}`);
    tbody.appendChild(row);
  }

  table.appendChild(tbody);
  block.appendChild(table);
  block.appendChild(
    el(
      "p",
      "analytics__note",
      `Projections are a least-squares fit over each account's current limit window, evaluated at ${formatInstant(now)}. ` +
        "They assume the observed burn rate continues unchanged.",
    ),
  );
  return block;
}

// --- Attribution -----------------------------------------------------------

function renderAttribution(attribution: AttributionResult): HTMLElement {
  const block = el("div", "analytics__block");
  block.setAttribute("data-testid", "attribution");
  block.appendChild(el("h3", "analytics__heading", "Per-repo attribution"));

  if (attribution.totalUsage <= 0) {
    block.appendChild(el("p", "analytics__note", "No usage observed in range."));
    return block;
  }

  const table = el("table", "table table--attribution");
  table.appendChild(headerRow(["Repository", "Usage", "Share", "Sweeps", "Models", "Accounts"]));
  const tbody = document.createElement("tbody");

  for (const repo of attribution.repos) {
    const row = document.createElement("tr");
    row.setAttribute("data-repo", repo.repo);
    cell(row, repo.repo, "cell--repo");
    cell(row, formatPercent(repo.usage, 2));
    cell(row, formatPercent(repo.share, 1));
    cell(row, String(repo.sweepCount));
    cell(row, repo.byModel.map((entry) => `${entry.name} ${formatPercent(entry.usage, 2)}`).join(", ") || UNKNOWN);
    cell(row, repo.byAccount.map((entry) => `${entry.name} ${formatPercent(entry.usage, 2)}`).join(", ") || UNKNOWN);
    tbody.appendChild(row);
  }

  if (attribution.unattributedUsage > 0) {
    const row = document.createElement("tr");
    row.className = "row--unattributed";
    row.setAttribute("data-repo", "(unattributed)");
    cell(row, "(unattributed)", "cell--repo");
    cell(row, formatPercent(attribution.unattributedUsage, 2));
    cell(row, formatPercent(attribution.unattributedUsage / attribution.totalUsage, 1));
    cell(row, UNKNOWN);
    cell(row, UNKNOWN);
    cell(row, UNKNOWN);
    tbody.appendChild(row);
  }

  table.appendChild(tbody);
  block.appendChild(table);
  block.appendChild(
    el(
      "p",
      "analytics__note",
      "Usage is in limit-window fractions (1.00 = one account's full window), not tokens or dollars — " +
        "the telemetry reports no absolute counts. tokens.snapshot carries no repo, so usage is joined to " +
        "sweeps by host and time overlap and split across concurrent sweeps in proportion to overlap " +
        "duration. Usage during intervals with no running sweep (crons, manual sessions) is reported as " +
        "unattributed rather than redistributed.",
    ),
  );
  block.appendChild(
    el(
      "p",
      "analytics__note",
      `Coverage: ${attribution.attributedIntervals} attributed intervals, ` +
        `${attribution.droppedIntervals} dropped across telemetry gaps, ` +
        `${attribution.rolloverIntervals} limit-window rollovers skipped.`,
    ),
  );
  return block;
}

// ---------------------------------------------------------------------------
// Mount
// ---------------------------------------------------------------------------

export interface MountOptions extends RenderOptions {
  /** How far back to pull history. Default 24 h. */
  lookbackMs?: number;
  fetchImpl?: typeof fetch;
  signal?: AbortSignal;
}

const DEFAULT_LOOKBACK_MS = 24 * 60 * 60 * 1000;

/**
 * Fetch, compute and render the panel. The one call an app shell needs.
 *
 * On a non-authenticated surface it renders the withheld notice and **makes no
 * request at all** — the panel does not merely hide data it already fetched.
 */
export async function mountTokenAnalytics(
  container: HTMLElement,
  options: MountOptions = {},
): Promise<TokenAnalytics | undefined> {
  const surface = options.surface ?? REQUIRED_SURFACE;
  if (surface !== REQUIRED_SURFACE) {
    container.replaceChildren(renderWithheldNotice());
    return undefined;
  }

  const now = options.now ?? Date.now();
  container.replaceChildren(el("p", "analytics__note", "Loading token analytics…"));

  let records: HistoryEnvelope[];
  try {
    records = await fetchHistory({
      since: now - (options.lookbackMs ?? DEFAULT_LOOKBACK_MS),
      fetchImpl: options.fetchImpl,
      signal: options.signal,
    });
  } catch (error) {
    container.replaceChildren(
      el("p", "analytics__error", `Could not load token analytics: ${(error as Error).message}`),
    );
    return undefined;
  }

  const analytics = computeTokenAnalytics(records, { now });
  renderTokenAnalytics(container, analytics, { surface, now });
  return analytics;
}
