/**
 * The token/cost analytics panel: burn curves, limit-window forecasts, and
 * per-repo attribution (Epic #4702, Phase 3, issue #4752).
 *
 * ## Public-exposure decision: pool-level aggregate, not per-account detail
 *
 * Operator decision (2026-07-31, issue #4847): **the signed-in dashboard
 * shows per-account token detail; the public view shows pool-level aggregate
 * stats instead of nothing.** This supersedes issue #4752's original
 * decision, which withheld the whole panel from the public surface. Phase
 * 2's redaction policy (`../../src/redaction.ts`) already draws the line
 * this module now follows: `/public/history`'s `tokens.snapshot` carries no
 * `accounts[]` at all — `deriveTokenPoolAggregate` replaces it with a
 * non-identifying `account_count` / `exhausted_count` / `mean_usage_fraction`
 * / `max_usage_fraction` / `next_limit_window_reset_at` summary — so a public
 * render of that data can never surface an account identifier; there is
 * nothing here to redact in the UI layer, only something to compute (a
 * pool-level burn series, `burn.ts`'s `buildPoolBurnCurves`) that the
 * original per-account modules cannot produce.
 *
 * What still does not render publicly, and why each one stays that way:
 *
 * 1. **Per-repo attribution is a repo-name table by construction.** The whole
 *    output of `attribution.ts` is "which repositories consumed the fleet's
 *    quota", and repo names are precisely what Phase 2 redacts from
 *    `/public/history` for private-visibility sweeps. Rendering this panel
 *    publicly would reconstruct, by inference from timing, the exact fact the
 *    redaction layer removes.
 * 2. **Per-account forecasts are a scheduling signal keyed to an identity.**
 *    "This fleet runs dry in 40 minutes" is one thing; "*agent-3* runs dry in
 *    40 minutes" ties that to operator infrastructure. The pool-level summary
 *    below reports the same risk (`exhausted_count`, `max_usage_fraction`)
 *    without naming which account it is — see `forecast.ts`'s
 *    `summarizePoolHealth` for why this is a summary and not a projection.
 *
 * `renderTokenAnalytics` renders the pool-level blocks in place of the
 * withheld notice on a `"public"` surface, and keeps the notice only for the
 * two blocks above. `mountTokenAnalytics` fetches from `/public/history`
 * (never `/api/*`) on that surface — see `api.ts`'s module doc for why that
 * is still a real boundary and not merely cosmetic. Coordinated with the
 * public view page (#4753) via `../../docs/token-analytics.md`.
 */

import type { AccountBurnCurve, BurnSegment, PoolBurnCurve, PoolBurnPoint, PoolBurnSegment } from "./burn.js";
import { buildBurnCurves, buildPoolBurnCurves } from "./burn.js";
import type { AccountForecast, ForecastStatus, PoolHealthSummary } from "./forecast.js";
import { forecastAccounts, summarizePoolHealths } from "./forecast.js";
import type { AttributionResult } from "./attribution.js";
import { attributeUsageToRepos } from "./attribution.js";
import { parsePoolSamples, parseSweepWindows, parseTokenSamples } from "./parse.js";
import type { HistoryEnvelope, PoolSample, SweepWindow, TokenSample } from "./types.js";
import { fetchHistory } from "./api.js";
import { formatDuration, formatInstant, formatPercent, formatRatePerHour, formatRelative, UNKNOWN } from "./format.js";

/** Which route surface the panel is being rendered on. */
export type DashboardSurface = "authenticated" | "public";

/** The surface a caller gets when it does not specify one. Authenticated,
 * not fail-safe-to-public: every real caller (`bootstrap.ts`'s
 * `startTokenAnalytics`) always passes an explicit surface derived from the
 * server-injected auth state, so this default only matters to a test or a
 * future caller that forgot to — and both surfaces now render real content
 * (see the module doc), so there is no "withheld by default" safety margin
 * this default used to buy. */
export const DEFAULT_SURFACE: DashboardSurface = "authenticated";

export interface TokenAnalytics {
  curves: AccountBurnCurve[];
  forecasts: AccountForecast[];
  attribution: AttributionResult;
  samples: TokenSample[];
  sweeps: SweepWindow[];
  /** Pool-level counterparts of `samples`/`curves`, built from the
   * `/public/history` aggregate shape (issue #4847). Populated from whichever
   * `tokens.snapshot` shape the input records actually carry — empty when
   * every record was the per-account shape, and vice versa. */
  poolSamples: PoolSample[];
  poolCurves: PoolBurnCurve[];
  poolHealth: PoolHealthSummary[];
}

export interface ComputeOptions {
  /** Reference instant (epoch ms); injectable for deterministic tests. */
  now?: number;
}

/** Partition a history page into its record families and run every analytic
 * over them — both the per-account family and the pool-aggregate family, so
 * this one function serves either surface. Pure — no DOM, no network — so the
 * whole computation is unit-testable from fixtures. */
export function computeTokenAnalytics(
  records: readonly HistoryEnvelope[],
  options: ComputeOptions = {},
): TokenAnalytics {
  const samples = parseTokenSamples(records);
  const poolSamples = parsePoolSamples(records);
  const sweeps = parseSweepWindows(records);
  const curves = buildBurnCurves(samples);
  const poolCurves = buildPoolBurnCurves(poolSamples);
  return {
    curves,
    forecasts: forecastAccounts(curves, { now: options.now }),
    attribution: attributeUsageToRepos(samples, sweeps, { now: options.now }),
    samples,
    sweeps,
    poolSamples,
    poolCurves,
    poolHealth: summarizePoolHealths(poolCurves),
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
 * Always renders `true` — every surface gets real content now (see the
 * module doc). On a `"public"` surface it renders the pool-level blocks
 * (`analytics.poolCurves` / `analytics.poolHealth`) plus a short notice
 * naming the two blocks that stay operator-only, instead of the per-account
 * burn curves, forecasts and attribution table an authenticated surface gets.
 */
export function renderTokenAnalytics(
  container: HTMLElement,
  analytics: TokenAnalytics,
  options: RenderOptions = {},
): boolean {
  const surface = options.surface ?? DEFAULT_SURFACE;
  container.replaceChildren();

  const now = options.now ?? Date.now();
  const section = el("section", "analytics");
  section.appendChild(el("h2", "analytics__title", "Token & cost analytics"));

  if (surface === "public") {
    section.appendChild(renderPoolBurn(analytics.poolCurves));
    section.appendChild(renderPoolHealth(analytics.poolHealth));
    section.appendChild(renderOperatorOnlyNotice());
    container.appendChild(section);
    return true;
  }

  section.appendChild(renderBurnCurves(analytics.curves));
  section.appendChild(renderForecasts(analytics.forecasts, now));
  section.appendChild(renderAttribution(analytics.attribution));
  container.appendChild(section);
  return true;
}

/** The block that replaces `analytics--withheld`'s old full-panel notice: the
 * public surface now renders real content above this, so the notice only has
 * to explain the two blocks that are still missing (see the module doc). */
function renderOperatorOnlyNotice(): HTMLElement {
  const notice = el(
    "p",
    "analytics__note analytics__note--withheld",
    "Per-repo attribution and per-account exhaustion forecasts are operator-only — they name " +
      "repositories and individual accounts. Sign in for the full breakdown.",
  );
  notice.setAttribute("data-testid", "analytics-operator-only-notice");
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

/** "studio" for one host, "studio +2" beyond that — the card has room for a
 * name, not a list, and the full set is on `data-hosts`. */
function formatHosts(hostIds: readonly string[]): string {
  if (hostIds.length === 0) return "—";
  const [first] = hostIds;
  return hostIds.length === 1 ? String(first) : `${first} +${hostIds.length - 1}`;
}

function renderBurnCard(curve: AccountBurnCurve): HTMLElement {
  const exhausted = curve.exhausted;
  const card = el("article", `burn-card${exhausted ? " burn-card--exhausted" : ""}`);
  card.setAttribute("data-account", curve.account);
  card.setAttribute("data-hosts", curve.hostIds.join(","));
  card.setAttribute("data-exhausted", String(exhausted));

  const head = el("header", "burn-card__head");
  head.appendChild(el("span", "burn-card__account", curve.account));
  // Provenance, not identity: one account, however many hosts reported it.
  head.appendChild(el("span", "burn-card__host", formatHosts(curve.hostIds)));
  if (curve.divergentHosts.length > 0) {
    // Same local name, contradictory readings — plausibly different upstream
    // accounts merged into one series. Say so rather than quietly averaging
    // two unrelated quotas (see burn.ts's findDivergentHosts).
    const warn = el("span", "badge badge--divergent", "CONFLICTING");
    warn.setAttribute("data-testid", "divergent-hosts");
    warn.title = `Hosts disagree on this account's usage: ${curve.divergentHosts.join(", ")}. It may name different accounts on different hosts.`;
    head.appendChild(warn);
  }
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

// --- Pool-level burn (public surface — issue #4847) -------------------------

/** The public-surface counterpart of `renderBurnCurves`: one card per host,
 * built from the pool aggregate rather than per-account curves — see the
 * module doc for what "pool-level" means here and why it names no account. */
function renderPoolBurn(curves: readonly PoolBurnCurve[]): HTMLElement {
  const block = el("div", "analytics__block");
  block.setAttribute("data-testid", "pool-burn");
  block.appendChild(el("h3", "analytics__heading", "Fleet token-pool load"));

  if (curves.length === 0) {
    block.appendChild(el("p", "analytics__note", "No tokens.snapshot history in range."));
    return block;
  }

  const list = el("div", "burn-grid");
  for (const curve of curves) list.appendChild(renderPoolBurnCard(curve));
  block.appendChild(list);
  block.appendChild(
    el(
      "p",
      "analytics__note",
      "Mean and peak usage across every account in this host's pool — never any one account's. " +
        "Peak usage is the solid line, mean usage the dashed one.",
    ),
  );
  return block;
}

function renderPoolBurnCard(curve: PoolBurnCurve): HTMLElement {
  const exhausted = curve.exhaustedCount > 0;
  const card = el("article", `burn-card${exhausted ? " burn-card--exhausted" : ""}`);
  card.setAttribute("data-host", curve.hostId);
  card.setAttribute("data-exhausted-count", String(curve.exhaustedCount));
  card.setAttribute("data-account-count", String(curve.accountCount));

  const head = el("header", "burn-card__head");
  head.appendChild(el("span", "burn-card__account", curve.hostId));
  if (exhausted) {
    const badge = el("span", "badge badge--exhausted", `${curve.exhaustedCount}/${curve.accountCount} exhausted`);
    badge.setAttribute("data-testid", "pool-exhausted-badge");
    badge.setAttribute("role", "status");
    head.appendChild(badge);
  }
  card.appendChild(head);

  card.appendChild(renderPoolSparkline(curve));

  const latest = curve.points[curve.points.length - 1];
  const foot = el("footer", "burn-card__foot");
  foot.appendChild(el("span", "burn-card__usage", `peak ${formatPercent(latest?.maxUsageFraction)}`));
  foot.appendChild(el("span", "burn-card__usage", `mean ${formatPercent(latest?.meanUsageFraction)}`));
  foot.appendChild(el("span", "burn-card__at", formatInstant(curve.latestAt || undefined)));
  card.appendChild(foot);
  return card;
}

/** Pool-level counterpart of `renderSparkline`: two lines per segment (peak,
 * mean) instead of one, same `[0, 1]`-pinned y-axis and per-segment breaks. */
function renderPoolSparkline(curve: PoolBurnCurve): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "sparkline");
  svg.setAttribute("viewBox", `0 0 ${SPARK_WIDTH} ${SPARK_HEIGHT}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("preserveAspectRatio", "none");
  const latest = curve.points[curve.points.length - 1];
  svg.setAttribute(
    "aria-label",
    `${curve.hostId} pool usage over time, latest peak ${formatPercent(latest?.maxUsageFraction)}, ` +
      `mean ${formatPercent(latest?.meanUsageFraction)}`,
  );

  const first = curve.points[0];
  const last = curve.points[curve.points.length - 1];
  if (!first || !last) return svg;

  const span = Math.max(last.at - first.at, 1);
  const x = (at: number): number => ((at - first.at) / span) * SPARK_WIDTH;
  const y = (usage: number): number => SPARK_HEIGHT - usage * SPARK_HEIGHT;

  for (const segment of curve.segments) {
    svg.appendChild(renderPoolSegmentLine(segment, x, y, (point) => point.maxUsageFraction, "sparkline__line"));
    svg.appendChild(
      renderPoolSegmentLine(segment, x, y, (point) => point.meanUsageFraction, "sparkline__line sparkline__line--mean"),
    );
  }
  return svg;
}

function renderPoolSegmentLine(
  segment: PoolBurnSegment,
  x: (at: number) => number,
  y: (usage: number) => number,
  select: (point: PoolBurnPoint) => number | undefined,
  className: string,
): SVGPolylineElement {
  const line = document.createElementNS(SVG_NS, "polyline");
  line.setAttribute("class", className);

  const usable: Array<{ at: number; value: number }> = [];
  for (const point of segment.points) {
    const value = select(point);
    if (value !== undefined) usable.push({ at: point.at, value });
  }
  // A single usable point has no line to draw; duplicating it renders a
  // visible dot rather than silently vanishing (mirrors `renderSegmentLine`).
  const only = usable[0];
  const points = usable.length === 1 && only ? [only, only] : usable;
  line.setAttribute("points", points.map((point) => `${x(point.at).toFixed(2)},${y(point.value).toFixed(2)}`).join(" "));
  return line;
}

/** The public-surface counterpart of `renderForecasts`: pool exhaustion state
 * *as measured*, never projected — see `forecast.ts`'s `summarizePoolHealth`
 * module doc for why a mean-based ETA is not drawn here. */
function renderPoolHealth(summaries: readonly PoolHealthSummary[]): HTMLElement {
  const block = el("div", "analytics__block");
  block.setAttribute("data-testid", "pool-health");
  block.appendChild(el("h3", "analytics__heading", "Pool exhaustion"));

  if (summaries.length === 0) {
    block.appendChild(el("p", "analytics__note", "No accounts observed in range."));
    return block;
  }

  const table = el("table", "table table--pool-health");
  table.appendChild(headerRow(["Host", "Accounts", "Exhausted", "Peak usage", "Mean usage", "Capacity returns"]));
  const tbody = document.createElement("tbody");

  for (const summary of summaries) {
    const row = document.createElement("tr");
    row.setAttribute("data-host", summary.hostId);
    if (summary.exhaustedFraction !== undefined && summary.exhaustedFraction > 0) row.className = "row--at-risk";
    cell(row, summary.hostId, "cell--host");
    cell(row, String(summary.accountCount));
    cell(
      row,
      summary.exhaustedFraction === undefined
        ? UNKNOWN
        : `${summary.exhaustedCount} (${formatPercent(summary.exhaustedFraction)})`,
    );
    cell(row, formatPercent(summary.maxUsageFraction));
    cell(row, formatPercent(summary.meanUsageFraction));
    // "Capacity returns" names no account — the earliest reset across the
    // whole pool — so it is safe on the public surface even though no
    // per-account forecast is drawn (see the note below the table).
    cell(row, formatInstant(summary.nextLimitWindowResetAt));
    tbody.appendChild(row);
  }

  table.appendChild(tbody);
  block.appendChild(table);
  const note = el(
    "p",
    "analytics__note",
    "No exhaustion timing is projected here: a pool-wide mean can sit comfortably mid-range while one " +
      "account is a sample away from running dry, and averaging that away would make a forecast actively " +
      "misleading. Sign in for a per-account burn-rate projection.",
  );
  note.setAttribute("data-testid", "pool-forecast-decision");
  block.appendChild(note);
  return block;
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
    cell(row, formatHosts(forecast.hostIds), "cell--host");
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
 * Fetches from `/public/history` on a `"public"` surface, `/api/history`
 * otherwise (`api.ts`'s `surface` option) — never the other route, on either
 * surface. Both surfaces render real content (see the module doc); there is
 * no longer a surface that renders without fetching.
 */
export async function mountTokenAnalytics(
  container: HTMLElement,
  options: MountOptions = {},
): Promise<TokenAnalytics | undefined> {
  const surface = options.surface ?? DEFAULT_SURFACE;
  const now = options.now ?? Date.now();
  container.replaceChildren(el("p", "analytics__note", "Loading token analytics…"));

  let records: HistoryEnvelope[];
  try {
    records = await fetchHistory({
      since: now - (options.lookbackMs ?? DEFAULT_LOOKBACK_MS),
      fetchImpl: options.fetchImpl,
      signal: options.signal,
      surface,
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
