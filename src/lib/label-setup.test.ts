import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  formatSetupResult,
  type LabelSetupResult,
  LOOM_LABELS,
  setupLoomLabels,
} from "./label-setup";

// Mock Tauri invoke
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

describe("label-setup", () => {
  let invokeMock: ReturnType<typeof vi.fn>;

  beforeEach(async () => {
    const { invoke } = await import("@tauri-apps/api/tauri");
    invokeMock = vi.mocked(invoke);
    invokeMock.mockClear();
  });

  describe("LOOM_LABELS", () => {
    it("contains all required issue labels", () => {
      const labelNames = LOOM_LABELS.map((l) => l.name);

      expect(labelNames).toContain("loom:proposal");
      expect(labelNames).toContain("loom:critic-suggestion");
      expect(labelNames).toContain("loom:ready");
      expect(labelNames).toContain("loom:in-progress");
      expect(labelNames).toContain("loom:blocked");
      expect(labelNames).toContain("loom:urgent");
    });

    it("contains all required PR labels", () => {
      const labelNames = LOOM_LABELS.map((l) => l.name);

      expect(labelNames).toContain("loom:review-requested");
      expect(labelNames).toContain("loom:changes-requested");
      expect(labelNames).toContain("loom:pr");
    });

    it("has valid color codes for all labels", () => {
      for (const label of LOOM_LABELS) {
        expect(label.color).toMatch(/^[0-9A-F]{6}$/i);
      }
    });

    it("has description for all labels", () => {
      for (const label of LOOM_LABELS) {
        expect(label.description).toBeTruthy();
        expect(label.description.length).toBeGreaterThan(10);
      }
    });
  });

  describe("setupLoomLabels", () => {
    it("creates all labels when none exist", async () => {
      invokeMock
        .mockResolvedValueOnce(true) // check_github_remote
        .mockResolvedValue(false); // check_label_exists (all labels don't exist)

      const result = await setupLoomLabels();

      expect(result.created.length).toBe(9); // All 9 labels
      expect(result.updated.length).toBe(0);
      expect(result.skipped.length).toBe(0);
      expect(result.errors.length).toBe(0);
    });

    it("skips existing labels when force=false", async () => {
      invokeMock
        .mockResolvedValueOnce(true) // check_github_remote
        .mockResolvedValue(true); // check_label_exists (all labels exist)

      const result = await setupLoomLabels(false);

      expect(result.created.length).toBe(0);
      expect(result.updated.length).toBe(0);
      expect(result.skipped.length).toBe(9); // All 9 labels skipped
      expect(result.errors.length).toBe(0);
    });

    it("updates existing labels when force=true", async () => {
      invokeMock
        .mockResolvedValueOnce(true) // check_github_remote
        .mockResolvedValue(true); // check_label_exists (all labels exist)

      const result = await setupLoomLabels(true);

      expect(result.created.length).toBe(0);
      expect(result.updated.length).toBe(9); // All 9 labels updated
      expect(result.skipped.length).toBe(0);
      expect(result.errors.length).toBe(0);
    });

    it("returns error when not in GitHub repository", async () => {
      invokeMock.mockResolvedValueOnce(false); // check_github_remote

      const result = await setupLoomLabels();

      expect(result.errors.length).toBe(1);
      expect(result.errors[0].label).toBe("all");
      expect(result.errors[0].error).toContain("GitHub remote");
    });

    it("continues processing labels even if one fails", async () => {
      invokeMock
        .mockResolvedValueOnce(true) // check_github_remote
        .mockResolvedValueOnce(false) // check_label_exists for first label
        .mockRejectedValueOnce(new Error("API Error")) // create_github_label fails for first
        .mockResolvedValueOnce(false) // check_label_exists for second label
        .mockResolvedValueOnce(undefined); // create_github_label succeeds for second

      const result = await setupLoomLabels();

      expect(result.errors.length).toBeGreaterThan(0);
      expect(result.created.length).toBeGreaterThan(0);
    });

    it("handles check_github_remote error gracefully", async () => {
      invokeMock.mockRejectedValueOnce(new Error("Failed to check remote"));

      const result = await setupLoomLabels();

      expect(result.errors.length).toBe(1);
      expect(result.errors[0].error).toContain("Failed to check remote");
    });
  });

  describe("formatSetupResult", () => {
    it("formats created labels", () => {
      const result: LabelSetupResult = {
        created: ["loom:ready", "loom:in-progress"],
        updated: [],
        skipped: [],
        errors: [],
      };

      const formatted = formatSetupResult(result);

      expect(formatted).toContain("Created 2 labels");
      expect(formatted).toContain("loom:ready");
      expect(formatted).toContain("loom:in-progress");
    });

    it("formats updated labels", () => {
      const result: LabelSetupResult = {
        created: [],
        updated: ["loom:ready"],
        skipped: [],
        errors: [],
      };

      const formatted = formatSetupResult(result);

      expect(formatted).toContain("Updated 1 label");
      expect(formatted).toContain("loom:ready");
    });

    it("formats skipped labels", () => {
      const result: LabelSetupResult = {
        created: [],
        updated: [],
        skipped: ["loom:ready", "loom:in-progress", "loom:blocked"],
        errors: [],
      };

      const formatted = formatSetupResult(result);

      expect(formatted).toContain("Skipped 3 existing labels");
      expect(formatted).toContain("loom:ready");
    });

    it("formats errors", () => {
      const result: LabelSetupResult = {
        created: [],
        updated: [],
        skipped: [],
        errors: [
          { label: "loom:ready", error: "API rate limit exceeded" },
          { label: "loom:in-progress", error: "Network error" },
        ],
      };

      const formatted = formatSetupResult(result);

      expect(formatted).toContain("Failed 2 labels");
      expect(formatted).toContain("loom:ready: API rate limit exceeded");
      expect(formatted).toContain("loom:in-progress: Network error");
    });

    it("formats mixed results", () => {
      const result: LabelSetupResult = {
        created: ["loom:ready"],
        updated: ["loom:in-progress"],
        skipped: ["loom:blocked"],
        errors: [{ label: "loom:urgent", error: "Failed" }],
      };

      const formatted = formatSetupResult(result);

      expect(formatted).toContain("Created 1 label");
      expect(formatted).toContain("Updated 1 label");
      expect(formatted).toContain("Skipped 1 existing label");
      expect(formatted).toContain("Failed 1 label");
    });

    it("returns empty string for empty result", () => {
      const result: LabelSetupResult = {
        created: [],
        updated: [],
        skipped: [],
        errors: [],
      };

      const formatted = formatSetupResult(result);

      expect(formatted).toBe("");
    });
  });
});
