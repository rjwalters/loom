/**
 * The Questions panel (#9906): confidence rendering, verdict wording rules,
 * and the index/detail render contract.
 */

import { describe, expect, it, vi } from "vitest";

import {
  confidenceOpacity,
  evidenceStatusPhrase,
  mountQuestionsPanel,
  parseDetailId,
  questionHash,
  renderQuestions,
  verdictGloss,
  type QuestionApiView,
} from "../src/questionsPanel";

function view(overrides: Partial<QuestionApiView> = {}): QuestionApiView {
  return {
    id: "augment-collision-value",
    version: 1,
    question: "Are Augment conflict predictions right often enough to save more time than they cost?",
    proposition: "…",
    yes_criteria: "…",
    no_criteria: "…",
    evidence_status_rule: "Confidence is the model's stated confidence in this assessment.",
    answer: {
      answer: "NO",
      confidence_pct: 80,
      summary: "Only a small retrospective study exists; no cost/savings evidence yet.",
      change_summary: "Unchanged from the first assessment.",
      evidence_status: "limited",
      missing_evidence: ["prospective shadow-study observations"],
      evidence_cutoff: "2026-10-01T00:00:00Z",
      model_id: "gemini-2.0-flash",
      run_kind: "scheduled",
      recorded_at: "2026-10-01T06:00:00Z",
    },
    history: [{ answer_date: "2026-10-01", answer: "NO", confidence_pct: 80, run_kind: "scheduled" }],
    arriving_sources: [{ label: "#9787", url: "https://github.com/rjwalters/loom/issues/9787" }],
    ...overrides,
  };
}

describe("confidenceOpacity", () => {
  it("maps the [0,100] endpoints to a readable, non-absolute band", () => {
    expect(confidenceOpacity(0)).toBeCloseTo(0.45);
    expect(confidenceOpacity(100)).toBeCloseTo(1.0);
    expect(confidenceOpacity(50)).toBeCloseTo(0.725);
  });

  it("clamps out-of-range input instead of producing invisible text", () => {
    expect(confidenceOpacity(-20)).toBe(confidenceOpacity(0));
    expect(confidenceOpacity(140)).toBe(confidenceOpacity(100));
  });
});

describe("verdictGloss", () => {
  it("never lets a limited-evidence NO read as 'proven ineffective'", () => {
    const gloss = verdictGloss("NO", "limited");
    expect(gloss).toContain("not yet demonstrated");
    expect(gloss).toContain("not a claim that it is disproven");
  });

  it("uses plain shown-does-not-hold wording only for sufficient evidence", () => {
    expect(verdictGloss("NO", "sufficient")).toContain("does not hold");
  });

  it("phrases YES as evidence-supported", () => {
    expect(verdictGloss("YES", "limited")).toContain("supports this today");
  });
});

describe("evidenceStatusPhrase", () => {
  it("covers the known statuses and falls back for unknown ones", () => {
    expect(evidenceStatusPhrase("limited")).toContain("cost/savings evidence is still missing");
    expect(evidenceStatusPhrase("mystery")).toContain("mystery");
  });
});

describe("hash helpers", () => {
  it("round-trips the detail deep link", () => {
    expect(questionHash("augment-collision-value")).toBe("#/questions/augment-collision-value");
    expect(parseDetailId("#/questions/augment-collision-value")).toBe("augment-collision-value");
    expect(parseDetailId("#/questions")).toBeUndefined();
    expect(parseDetailId("#/hosts/x")).toBeUndefined();
  });
});

describe("renderQuestions", () => {
  it("detail shows the verdict, summary, arriving-later links, and history", () => {
    const root = document.createElement("div");
    renderQuestions(root, [view()], "augment-collision-value");
    const verdict = root.querySelector('[data-testid="question-verdict"]');
    expect(verdict?.textContent).toContain("NO");
    expect(verdict?.textContent).toContain("80%");
    expect(root.querySelector('[data-testid="question-summary"]')?.textContent).toContain(
      "no cost/savings evidence",
    );
    const arriving = root.querySelector('[data-testid="question-arriving"]');
    expect(arriving?.textContent).toContain("arrives later");
    expect(arriving?.querySelector("a")?.getAttribute("href")).toContain("issues/9787");
    expect(root.querySelector('[data-testid="question-history"]')?.textContent).toContain("2026-10-01");
  });

  it("applies confidence-scaled emphasis to the verdict badge", () => {
    const root = document.createElement("div");
    renderQuestions(root, [view({ answer: { ...view().answer!, confidence_pct: 20 } })], "augment-collision-value");
    const faded = (root.querySelector('[data-testid="question-verdict"]') as HTMLElement).style.opacity;
    renderQuestions(root, [view({ answer: { ...view().answer!, confidence_pct: 95 } })], "augment-collision-value");
    const strong = (root.querySelector('[data-testid="question-verdict"]') as HTMLElement).style.opacity;
    expect(Number(strong)).toBeGreaterThan(Number(faded));
  });

  it("renders an awaiting state instead of inventing an answer", () => {
    const root = document.createElement("div");
    renderQuestions(root, [view({ answer: null, history: [] })], "augment-collision-value");
    expect(root.querySelector('[data-testid="question-awaiting"]')?.textContent).toContain(
      "Awaiting first assessment",
    );
    expect(root.querySelector('[data-testid="question-verdict"]')).toBeNull();
  });

  it("index lists every question with its compact verdict", () => {
    const root = document.createElement("div");
    renderQuestions(root, [view()], undefined);
    const index = root.querySelector('[data-testid="question-index"]');
    expect(index?.querySelectorAll("li").length).toBe(1);
    expect(index?.textContent).toContain("NO · 80%");
  });
});

describe("mountQuestionsPanel", () => {
  it("fetches /api/questions and renders; teardown stops updates", async () => {
    const container = document.createElement("div");
    document.body.replaceChildren(container);
    const fetchImpl = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ questions: [view()] }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    );
    const teardown = mountQuestionsPanel(container, fetchImpl as unknown as typeof fetch);
    await vi.waitFor(() => {
      expect(container.querySelector('[data-testid="question-index"]')).not.toBeNull();
    });
    expect(fetchImpl).toHaveBeenCalledWith("/api/questions");
    // Detail navigation re-renders without a second fetch (the panel reads
    // the event's target URL, so a synthetic hashchange drives it).
    window.dispatchEvent(
      new HashChangeEvent("hashchange", {
        newURL: "https://dashboard.example/#/questions/augment-collision-value",
      }),
    );
    await vi.waitFor(() => {
      expect(container.querySelector('[data-testid="question-verdict"]')).not.toBeNull();
    });
    expect(fetchImpl).toHaveBeenCalledTimes(1);
    teardown();
    // After teardown the listener is gone: hash changes must not re-render.
    const html = container.innerHTML;
    window.dispatchEvent(
      new HashChangeEvent("hashchange", { newURL: "https://dashboard.example/#/questions" }),
    );
    expect(container.innerHTML).toBe(html);
  });

  it("shows a load-failure notice instead of a blank panel", async () => {
    const container = document.createElement("div");
    document.body.replaceChildren(container);
    const fetchImpl = vi.fn().mockRejectedValue(new Error("network down"));
    const teardown = mountQuestionsPanel(container, fetchImpl as unknown as typeof fetch);
    await vi.waitFor(() => {
      expect(container.querySelector('[data-testid="questions-error"]')?.textContent).toContain(
        "network down",
      );
    });
    teardown();
  });
});
