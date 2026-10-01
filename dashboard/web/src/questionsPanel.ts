/**
 * The Questions panel (#9906): the shared daily YES/NO surface.
 *
 * Every question here follows one format — a large **YES or NO** with a
 * **percentage confidence**, a short plain-language explanation authored by
 * the daily Gemini Flash assessment, evidence links, and preserved answer
 * history. The panel renders both the index (`#/questions`) and the detail
 * view (`#/questions/<id>`) — detail navigation is handled by the panel's
 * own hashchange listener so a route change between two question views
 * re-renders without remounting the panel (matching `App`'s
 * "mount on route *change*" contract from `panels.ts`).
 *
 * ## Confidence rendering
 *
 * The YES/NO badge is colored by verdict (green YES / red NO) and its
 * emphasis is scaled by confidence: high confidence is saturated, low
 * confidence fades toward neutral so a shaky verdict never looks certain.
 * Opacity is the mechanism (never color alone): the verdict text itself
 * stays fully readable at every level, and the confidence percentage is
 * always shown numerically for accessibility.
 *
 * ## The "arrives later" section
 *
 * Questions whose decisive evidence does not exist yet (the Augment
 * question's prospective shadow-study observations) render an explicit
 * "arrives later" block linking the issues that will produce it — the page
 * must not pretend the study already ran, and a NO there reads as "not yet
 * demonstrated", never "proven ineffective".
 */

import { el } from "./dom";

export interface QuestionAnswerApi {
  answer: "YES" | "NO";
  confidence_pct: number;
  summary: string;
  change_summary: string;
  evidence_status: string;
  missing_evidence: string[];
  evidence_cutoff: string;
  model_id: string;
  run_kind: string;
  recorded_at: string;
}

export interface QuestionApiView {
  id: string;
  version: number;
  question: string;
  proposition: string;
  yes_criteria: string;
  no_criteria: string;
  evidence_status_rule: string;
  answer: QuestionAnswerApi | null;
  history: { answer_date: string; answer: string; confidence_pct: number; run_kind: string }[];
  arriving_sources: { label: string; url: string }[];
}

/** Badge emphasis for a confidence level: 0 → faded, 100 → saturated.
 * Deliberately never reaches 0 (the verdict must stay readable) and never
 * 1 (absolute certainty is not claimed). */
export function confidenceOpacity(confidencePct: number): number {
  const clamped = Math.max(0, Math.min(100, confidencePct));
  return 0.45 + 0.55 * (clamped / 100);
}

/** Human phrasing for the evidence-status enum. */
export function evidenceStatusPhrase(status: string): string {
  switch (status) {
    case "sufficient":
      return "Evidence covers accuracy and measured costs.";
    case "limited":
      return "Evidence covers accuracy; cost/savings evidence is still missing.";
    case "insufficient":
      return "Not enough evidence yet to assess either side.";
    default:
      return `Evidence status: ${status}`;
  }
}

/** The one-line plain-language gloss for a verdict, enforcing the wording
 * rule that a NO here means "not yet demonstrated", never "proven
 * ineffective". */
export function verdictGloss(answer: "YES" | "NO", evidenceStatus: string): string {
  if (answer === "YES") return "Yes — the evidence supports this today.";
  if (evidenceStatus === "sufficient") {
    return "No — the evidence shows this does not hold today.";
  }
  return "No — not yet demonstrated by the evidence so far. (This is not a claim that it is disproven.)";
}

export function questionHash(id: string): string {
  return `#/questions/${encodeURIComponent(id)}`;
}

export function parseDetailId(hash: string): string | undefined {
  const match = /^#\/questions\/([a-z0-9-]+)$/.exec(hash);
  return match ? decodeURIComponent(match[1] ?? "") : undefined;
}

function verdictBadge(answer: QuestionAnswerApi): HTMLElement {
  const opacity = confidenceOpacity(answer.confidence_pct);
  const badge = el(
    "div",
    {
      class: `question-verdict question-verdict--${answer.answer.toLowerCase()}`,
      data: { testid: "question-verdict" },
      role: "status",
    },
    el("span", { class: "question-verdict__word" }, answer.answer),
    el("span", { class: "question-verdict__confidence" }, `${answer.confidence_pct}% confidence`),
  );
  badge.style.opacity = String(opacity);
  return badge;
}

function detailContent(view: QuestionApiView): HTMLElement {
  const children: Node[] = [el("h2", { class: "panel-route__title" }, view.question)];
  if (!view.answer) {
    children.push(
      el(
        "p",
        { class: "panel-route__note", data: { testid: "question-awaiting" } },
        "Awaiting first assessment — the daily evaluator has not published an answer for this question yet.",
      ),
    );
    return el("div", { class: "question-detail" }, ...children);
  }
  const a = view.answer;
  children.push(verdictBadge(a));
  children.push(
    el(
      "p",
      { class: "question-gloss", data: { testid: "question-gloss" } },
      verdictGloss(a.answer, a.evidence_status),
    ),
  );
  children.push(
    el("p", { class: "question-summary", data: { testid: "question-summary" } }, a.summary),
  );
  children.push(
    el("p", { class: "question-change", data: { testid: "question-change" } }, a.change_summary),
  );
  children.push(
    el(
      "p",
      { class: "question-meta", data: { testid: "question-meta" } },
      `Assessed by ${a.model_id} · updated ${a.recorded_at} · evidence through ${a.evidence_cutoff} · ${evidenceStatusPhrase(a.evidence_status)}`,
    ),
  );
  if (a.missing_evidence.length > 0) {
    children.push(
      el(
        "p",
        { class: "question-missing", data: { testid: "question-missing" } },
        `Missing evidence: ${a.missing_evidence.join("; ")}`,
      ),
    );
  }
  children.push(el("p", { class: "question-rule" }, view.evidence_status_rule));
  if (view.arriving_sources.length > 0) {
    children.push(
      el(
        "section",
        { class: "question-arriving", data: { testid: "question-arriving" } },
        el("h3", {}, "Evidence that arrives later"),
        el(
          "p",
          {},
          "The decisive cost/savings observations do not exist yet — they come from the prospective shadow study. Until then this answer reflects the retrospective evidence only:",
        ),
        el(
          "ul",
          {},
          ...view.arriving_sources.map((s) =>
            el("li", {}, el("a", { href: s.url }, s.label)),
          ),
        ),
      ),
    );
  }
  if (view.history.length > 0) {
    children.push(
      el(
        "section",
        { class: "question-history", data: { testid: "question-history" } },
        el("h3", {}, "Answer history"),
        el(
          "ul",
          {},
          ...view.history.map((h) =>
            el(
              "li",
              {},
              `${h.answer_date}: ${h.answer} (${h.confidence_pct}%${h.run_kind === "manual" ? ", manual refresh" : ""})`,
            ),
          ),
        ),
      ),
    );
  }
  children.push(
    el(
      "p",
      {},
      el("a", { href: "#/questions" }, "← All questions"),
    ),
  );
  return el("div", { class: "question-detail" }, ...children);
}

function indexContent(views: QuestionApiView[]): HTMLElement {
  const items = views.map((v) =>
    el(
      "li",
      { class: "question-index__item" },
      el(
        "a",
        { href: questionHash(v.id), data: { testid: `question-link-${v.id}` } },
        v.question,
      ),
      v.answer
        ? el(
            "span",
            { class: "question-index__answer" },
            `${v.answer.answer} · ${v.answer.confidence_pct}%`,
          )
        : el("span", { class: "question-index__answer" }, "Awaiting first assessment"),
      v.answer
        ? el("span", { class: "question-index__summary" }, v.answer.summary)
        : null,
    ),
  );
  return el(
    "div",
    { class: "questions-index" },
    el("h2", { class: "panel-route__title" }, "Questions"),
    el("p", { class: "panel-route__note" }, "Daily YES/NO assessments with confidence, authored from the cited evidence."),
    el("ul", { class: "question-index", data: { testid: "question-index" } }, ...items),
  );
}

export function renderQuestions(
  root: HTMLElement,
  views: QuestionApiView[],
  detailId: string | undefined,
): void {
  if (views.length === 0) {
    root.replaceChildren(
      el("div", { class: "questions-index" }, el("p", { class: "panel-route__note" }, "No questions configured.")),
    );
    return;
  }
  if (detailId) {
    const view = views.find((v) => v.id === detailId);
    if (!view) {
      root.replaceChildren(
        el(
          "div",
          { class: "question-detail" },
          el("p", { class: "panel-route__note" }, `Unknown question: ${detailId}`),
          el("p", {}, el("a", { href: "#/questions" }, "← All questions")),
        ),
      );
      return;
    }
    root.replaceChildren(detailContent(view));
    return;
  }
  root.replaceChildren(indexContent(views));
}

/** Mount the Questions panel: one fetch, index or detail by hash, and a
 * hashchange listener for detail navigation while mounted. Returns the
 * teardown. */
export function mountQuestionsPanel(root: HTMLElement, fetchImpl: typeof fetch = fetch): () => void {
  const container = el("div", { data: { testid: "questions-panel" } });
  root.replaceChildren(container);
  container.replaceChildren(el("p", { class: "panel-route__note" }, "Loading questions…"));

  let alive = true;
  /** The last successfully fetched views. Detail navigation re-renders from
   * this cache — the hashchange handler must not re-fetch (a second read of
   * the same assessment run adds nothing, and a cached Response body cannot
   * be consumed twice). Only the initial load fetches. */
  let cachedViews: QuestionApiView[] | undefined;
  /** `detailOverride` lets the hashchange handler pass the id parsed from
   * the event's own target URL — the source of truth for navigation — so a
   * synthetic/embedded context cannot disagree with `window.location`. */
  const renderFrom = (views: QuestionApiView[], detailOverride?: string) => {
    const detailId = detailOverride ?? parseDetailId(window.location.hash);
    renderQuestions(container, views, detailId);
  };
  const render = async (detailOverride?: string) => {
    try {
      const resp = await fetchImpl("/api/questions");
      if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
      const body = (await resp.json()) as { questions: QuestionApiView[] };
      if (!alive) return;
      cachedViews = body.questions;
      renderFrom(cachedViews, detailOverride);
    } catch (error) {
      if (!alive) return;
      container.replaceChildren(
        el(
          "p",
          { class: "panel-route__note", data: { testid: "questions-error" } },
          `Could not load questions: ${error instanceof Error ? error.message : String(error)}`,
        ),
      );
    }
  };
  /** Detail navigation while mounted reads the *event's* target URL — not a
   * re-read of `window.location` — so tests (and embedded contexts) can
   * drive navigation deterministically. */
  const onHashChange = (event: HashChangeEvent): void => {
    let hash = "";
    try {
      hash = new URL(event.newURL).hash;
    } catch {
      hash = window.location.hash;
    }
    const detailId = parseDetailId(hash);
    if (detailId || hash === "#/questions") {
      // Navigation re-renders from the cached views — no second fetch.
      if (cachedViews) renderFrom(cachedViews, detailId);
      else void render(detailId);
    }
  };
  window.addEventListener("hashchange", onHashChange);
  void render();
  return () => {
    alive = false;
    window.removeEventListener("hashchange", onHashChange);
  };
}
