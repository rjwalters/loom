/**
 * "Elastic spend this period" — `ephemeral_compute` cost over a selectable
 * window (Issue #8306, Phase 3 of #8257).
 *
 * ## Why this is not part of `analytics/`
 *
 * `analytics/` reconstructs a *derived* quantity: token `usage_fraction` has
 * to be segmented at limit-window rollovers, fitted, and attributed back to
 * repos across a join it has no key for (`analytics/attribution.ts`). None of
 * that applies here — `estimated_cost_usd` is a direct field on a completion
 * record, so "spend this period" is one `SUM` the backend already does
 * (`../../src/query.ts`'s `queryElasticSpend`). Putting a sum-of-a-column next
 * to a least-squares exhaustion forecast would import the whole burn-curve
 * vocabulary to describe something that has none of its problems. What this
 * *does* mirror is that panel's shape: a period selector, a headline figure,
 * a per-bucket breakdown, and an explicit empty state.
 *
 * ## The daily ceiling
 *
 * The reference operator runs a standing spot budget of
 * {@link DEFAULT_DAILY_CEILING_USD}/day. A window total alone cannot answer
 * the question that ceiling poses — "did any day breach it" — so every day
 * renders as a bar proportional to the ceiling, and a day at or over it is
 * flagged. The ceiling is an option with a documented default rather than a
 * hardcoded constant: it is one deployment's budget, not a property of Loom.
 *
 * ## Public surface
 *
 * `/public/spend` answers `{ withheld: true }` with no numbers at all — no
 * `ephemeral_compute` field survives redaction, and a zeroed summary would
 * read as a real idle window rather than as withheld
 * (`../../src/redaction.ts`). This panel renders that response as an explicit
 * operator-only notice, never as `$0.00`.
 */

import { el, replaceChildren } from "./dom";
import { UNKNOWN, formatDuration } from "./format";
import type { FetchLike } from "./historyClient";

/** The reference deployment's standing per-day spot budget, in USD. Override
 * via {@link SpendPanelOptions.dailyCeilingUsd} for a fleet with a different
 * one; `null` renders the breakdown with no ceiling reference at all. */
export const DEFAULT_DAILY_CEILING_USD = 100;

/** One selectable period. `days` is the lookback from "now"; the backend
 * filters on `emitted_at`, so a window is a half-open `[since, now)`. */
export interface SpendWindow {
  id: string;
  label: string;
  days: number;
}

/** The period selector's options — deliberately a short fixed list rather
 * than a date picker. Elastic spend is checked against a daily budget, so the
 * useful questions are "today", "this week", "this month"; an arbitrary range
 * picker would add a calendar widget to answer a question nobody asks of a
 * spend ceiling. */
export const SPEND_WINDOWS: readonly SpendWindow[] = [
  { id: "1d", label: "Last 24 hours", days: 1 },
  { id: "7d", label: "Last 7 days", days: 7 },
  { id: "30d", label: "Last 30 days", days: 30 },
];

export const DEFAULT_SPEND_WINDOW_ID = "7d";

/** Resolve a window id to its definition, falling back to the default rather
 * than throwing — the id can come from a `<select>` whose options a future
 * edit changed out from under a stored preference. */
export function spendWindow(id: string): SpendWindow {
  return (
    SPEND_WINDOWS.find((window) => window.id === id) ??
    SPEND_WINDOWS.find((window) => window.id === DEFAULT_SPEND_WINDOW_ID) ??
    SPEND_WINDOWS[0]!
  );
}

export interface SpendDay {
  day: string;
  costUsd: number;
  jobCount: number;
}

/** The authenticated `/api/spend` body — mirrors `ElasticSpendSummary` in
 * `../../src/query.ts`. */
export interface SpendSummary {
  since: string | null;
  until: string | null;
  totalCostUsd: number;
  jobCount: number;
  totalWallClockSec: number | null;
  peakDailyCostUsd: number | null;
  days: SpendDay[];
  withheld?: false;
}

/** The `/public/spend` body — see the module doc. */
export interface WithheldSpend {
  since: string | null;
  until: string | null;
  withheld: true;
}

export type SpendResponse = SpendSummary | WithheldSpend;

export function isWithheld(response: SpendResponse): response is WithheldSpend {
  return (response as WithheldSpend).withheld === true;
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

function num(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/**
 * Narrow a `/api/spend` (or `/public/spend`) body.
 *
 * A body that is not an object, or that claims `withheld`, becomes the
 * withheld shape. Everything else is read field by field with a `0` floor on
 * the two counts — deliberately *not* the "unknown is not zero" treatment the
 * rest of this app gives measurements, because these are `SUM`/`COUNT`
 * outputs over a known row set: a malformed one means "the server's answer was
 * unreadable", and the surrounding shape (a present `days` array) already
 * distinguishes that from a withheld response. `totalWallClockSec` keeps the
 * nullable treatment, since the backend returns `null` there on purpose.
 */
export function parseSpendResponse(body: unknown): SpendResponse {
  if (typeof body !== "object" || body === null) {
    return { since: null, until: null, withheld: true };
  }
  const raw = body as Record<string, unknown>;
  const since = typeof raw.since === "string" ? raw.since : null;
  const until = typeof raw.until === "string" ? raw.until : null;
  if (raw.withheld === true) return { since, until, withheld: true };

  const days: SpendDay[] = [];
  if (Array.isArray(raw.days)) {
    for (const entry of raw.days) {
      if (typeof entry !== "object" || entry === null) continue;
      const row = entry as Record<string, unknown>;
      if (typeof row.day !== "string") continue;
      days.push({ day: row.day, costUsd: num(row.costUsd) ?? 0, jobCount: num(row.jobCount) ?? 0 });
    }
  }

  return {
    since,
    until,
    totalCostUsd: num(raw.totalCostUsd) ?? 0,
    jobCount: num(raw.jobCount) ?? 0,
    totalWallClockSec: num(raw.totalWallClockSec) ?? null,
    peakDailyCostUsd: num(raw.peakDailyCostUsd) ?? null,
    days,
  };
}

export interface FetchSpendOptions {
  /** `/api/spend` or `/public/spend`. */
  basePath: string;
  window: SpendWindow;
  /** Reference instant; injectable for deterministic tests. */
  now?: Date;
  fetchImpl?: FetchLike;
  signal?: AbortSignal;
}

/** Fetch one window's spend. Throws on a non-2xx response (the panel renders
 * the message); a body that does not parse degrades to the withheld shape
 * rather than throwing, so a mid-deploy schema skew shows a notice instead of
 * a blank panel. */
export async function fetchSpend(options: FetchSpendOptions): Promise<SpendResponse> {
  const doFetch = options.fetchImpl ?? (globalThis.fetch as unknown as FetchLike);
  const now = options.now ?? new Date();
  const since = new Date(now.getTime() - options.window.days * 24 * 60 * 60 * 1000);

  const params = new URLSearchParams({ since: since.toISOString(), until: now.toISOString() });
  const response = await doFetch(`${options.basePath}?${params.toString()}`, {
    signal: options.signal,
  });
  if (!response.ok) {
    throw new Error(`GET ${options.basePath} returned ${response.status}`);
  }
  return parseSpendResponse(await response.json());
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/** `$12.34`. Two decimals always — this is money, and `$12.3` reads as a
 * truncation bug. `—` for an unknown figure, never `$0.00`. */
export function formatUsd(value: number | null | undefined): string {
  if (typeof value !== "number" || !Number.isFinite(value)) return UNKNOWN;
  return `$${value.toFixed(2)}`;
}

/** Fraction of the ceiling a day's spend represents, clamped to `[0, 1]` for
 * bar geometry. `0` when there is no ceiling to compare against. */
export function ceilingFraction(costUsd: number, ceilingUsd: number | null): number {
  if (ceilingUsd === null || !(ceilingUsd > 0)) return 0;
  return Math.max(0, Math.min(1, costUsd / ceilingUsd));
}

export interface RenderSpendOptions {
  window: SpendWindow;
  /** See {@link DEFAULT_DAILY_CEILING_USD}. */
  dailyCeilingUsd?: number | null;
}

function summaryFields(summary: SpendSummary, ceilingUsd: number | null): HTMLElement {
  return el(
    "dl",
    { class: "spend__fields" },
    el("dt", { class: "field__label" }, "Total"),
    el("dd", { class: "field__value spend__total", data: { testid: "spend-total" } }, formatUsd(summary.totalCostUsd)),
    el("dt", { class: "field__label" }, "Completed jobs"),
    el("dd", { class: "field__value" }, String(summary.jobCount)),
    el("dt", { class: "field__label" }, "Compute time"),
    el("dd", { class: "field__value" }, formatDuration(summary.totalWallClockSec ?? undefined)),
    el("dt", { class: "field__label" }, "Peak day"),
    el(
      "dd",
      {
        class: `field__value${
          ceilingUsd !== null && (summary.peakDailyCostUsd ?? 0) >= ceilingUsd ? " spend--over-ceiling" : ""
        }`,
        data: { testid: "spend-peak-day" },
        title:
          ceilingUsd !== null
            ? `Standing daily ceiling ${formatUsd(ceilingUsd)}`
            : undefined,
      },
      ceilingUsd !== null
        ? `${formatUsd(summary.peakDailyCostUsd)} of ${formatUsd(ceilingUsd)}`
        : formatUsd(summary.peakDailyCostUsd),
    ),
  );
}

function dayRow(day: SpendDay, ceilingUsd: number | null): HTMLElement {
  const overCeiling = ceilingUsd !== null && day.costUsd >= ceilingUsd;
  return el(
    "li",
    {
      class: `spend-day${overCeiling ? " spend-day--over-ceiling" : ""}`,
      data: { testid: "spend-day", day: day.day, over: String(overCeiling) },
    },
    el("span", { class: "spend-day__date" }, day.day),
    el(
      "span",
      { class: "spend-day__bar" },
      el("span", {
        class: "spend-day__fill",
        // Inline geometry only — the width is data, not style (same carve-out
        // `dom.ts`'s `style` attr documents for the token usage meter).
        style: `width: ${(ceilingFraction(day.costUsd, ceilingUsd) * 100).toFixed(1)}%`,
      }),
    ),
    el("span", { class: "spend-day__cost" }, formatUsd(day.costUsd)),
    el("span", { class: "spend-day__jobs" }, `${day.jobCount} job${day.jobCount === 1 ? "" : "s"}`),
  );
}

/** The withheld notice, mirroring `views/runningCompute.ts`'s. */
function withheldNotice(): HTMLElement {
  return el(
    "p",
    { class: "spend__note spend__note--withheld", data: { testid: "spend-withheld" } },
    "Elastic compute spend is operator-only — an exact dollar figure for a private compute " +
      "fleet is not published. Sign in to see the breakdown.",
  );
}

/**
 * Render a spend response into `container`, replacing its contents.
 *
 * Three distinct states, none of which may be confused for another:
 * **withheld** (public surface), **no spend in this window** (a real, correct
 * `$0.00` — the window genuinely had no completed jobs), and **a breakdown**.
 * The zero case still shows the `$0.00` headline rather than only a note, so a
 * reader who came to check the budget gets the answer, with the note
 * explaining why the day list below it is empty.
 */
export function renderSpend(
  container: HTMLElement,
  response: SpendResponse,
  options: RenderSpendOptions,
): void {
  const ceilingUsd = options.dailyCeilingUsd === undefined ? DEFAULT_DAILY_CEILING_USD : options.dailyCeilingUsd;

  if (isWithheld(response)) {
    container.replaceChildren(withheldNotice());
    return;
  }

  replaceChildren(
    container,
    summaryFields(response, ceilingUsd),
    response.days.length === 0
      ? el(
          "p",
          { class: "spend__note", data: { testid: "spend-empty" } },
          `No ephemeral compute jobs completed in the ${options.window.label.toLowerCase()}.`,
        )
      : el(
          "ul",
          { class: "spend__days", data: { testid: "spend-days" } },
          response.days.map((day) => dayRow(day, ceilingUsd)),
        ),
    ceilingUsd !== null
      ? el(
          "p",
          { class: "spend__note" },
          `Bars are drawn against the standing daily ceiling of ${formatUsd(ceilingUsd)}.`,
        )
      : null,
  );
}

// ---------------------------------------------------------------------------
// The panel
// ---------------------------------------------------------------------------

export interface SpendPanelOptions {
  /** `/api/spend` or `/public/spend`. */
  basePath: string;
  container: HTMLElement;
  windowId?: string;
  dailyCeilingUsd?: number | null;
  now?: () => Date;
  fetchImpl?: FetchLike;
}

/**
 * Owns the period selector, the fetch, and the render — the same plain-DOM
 * ownership pattern `historicalChartsPanel.ts` and `liveFeedPanel.ts` use.
 *
 * The selector is built once in the constructor and the *results* region is
 * what `refresh()` replaces, so changing the period never rebuilds (and so
 * never resets the focus/selection of) the control that triggered the change.
 */
export class SpendPanel {
  private readonly basePath: string;
  private readonly container: HTMLElement;
  private readonly dailyCeilingUsd: number | null;
  private readonly now: () => Date;
  private readonly fetchImpl: FetchLike | undefined;
  private readonly results: HTMLElement;
  private readonly select: HTMLSelectElement;
  private windowId: string;

  constructor(options: SpendPanelOptions) {
    this.basePath = options.basePath;
    this.container = options.container;
    this.dailyCeilingUsd =
      options.dailyCeilingUsd === undefined ? DEFAULT_DAILY_CEILING_USD : options.dailyCeilingUsd;
    this.now = options.now ?? (() => new Date());
    this.fetchImpl = options.fetchImpl;
    this.windowId = spendWindow(options.windowId ?? DEFAULT_SPEND_WINDOW_ID).id;

    this.results = el("div", { class: "spend__results", data: { testid: "spend-results" } });
    this.select = document.createElement("select");
    this.select.className = "spend__period";
    this.select.setAttribute("data-testid", "spend-period");
    this.select.setAttribute("aria-label", "Spend period");
    for (const window of SPEND_WINDOWS) {
      const option = document.createElement("option");
      option.value = window.id;
      option.textContent = window.label;
      option.selected = window.id === this.windowId;
      this.select.appendChild(option);
    }
    this.select.addEventListener("change", () => {
      this.windowId = spendWindow(this.select.value).id;
      void this.refresh().catch((error: unknown) => this.renderError(error));
    });

    this.container.replaceChildren(
      el(
        "header",
        { class: "spend__header" },
        el("h2", { class: "spend__title" }, "Elastic spend this period"),
        this.select,
      ),
      this.results,
    );
  }

  /** The period currently selected. */
  get currentWindow(): SpendWindow {
    return spendWindow(this.windowId);
  }

  /** Fetch the selected window and re-render. Rejects on a transport/HTTP
   * failure so the caller (`panels.ts`) can route it to `renderMountError`;
   * the internal `change` handler routes it here instead, since by then there
   * is a panel on screen to put the message in. */
  async refresh(): Promise<void> {
    const response = await fetchSpend({
      basePath: this.basePath,
      window: this.currentWindow,
      now: this.now(),
      fetchImpl: this.fetchImpl,
    });
    renderSpend(this.results, response, {
      window: this.currentWindow,
      dailyCeilingUsd: this.dailyCeilingUsd,
    });
  }

  private renderError(error: unknown): void {
    const message = error instanceof Error ? error.message : String(error);
    this.results.replaceChildren(
      el("p", { class: "spend__note", data: { testid: "spend-error" } }, `Could not load: ${message}`),
    );
  }
}
