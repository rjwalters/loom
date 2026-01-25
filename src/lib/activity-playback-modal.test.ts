/**
 * Tests for Activity Playback Modal
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  showActivityPlaybackForIssue,
  showActivityPlaybackForPR,
  showActivityPlaybackModal,
  type TimelineEntry,
} from "./activity-playback-modal";
import { AppState, setAppState } from "./state";

// Mock localStorage before any imports that might use it
const localStorageMock = {
  getItem: vi.fn(),
  setItem: vi.fn(),
  removeItem: vi.fn(),
  clear: vi.fn(),
  length: 0,
  key: vi.fn(),
};
Object.defineProperty(globalThis, "localStorage", {
  value: localStorageMock,
  writable: true,
});

// Mock Tauri APIs
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({
  save: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-fs", () => ({
  writeTextFile: vi.fn(),
}));

vi.mock("./logger", () => ({
  Logger: {
    forComponent: () => ({
      info: vi.fn(),
      error: vi.fn(),
      warn: vi.fn(),
      debug: vi.fn(),
    }),
  },
}));

vi.mock("./toast", () => ({
  showToast: vi.fn(),
}));

describe("Activity Playback Modal", () => {
  let mockState: AppState;

  const mockTimelineEntries: TimelineEntry[] = [
    {
      id: 1,
      timestamp: "2026-01-24T10:00:00Z",
      role: "builder",
      action: "Claimed issue",
      duration_ms: 5000,
      outcome: "success",
      issue_number: 42,
      pr_number: null,
      prompt_preview: "Implement feature X",
      output_preview: null,
      tokens: 1500,
      cost: 0.05,
      event_type: "issue_claimed",
      label_before: "loom:issue",
      label_after: "loom:building",
    },
    {
      id: 2,
      timestamp: "2026-01-24T10:30:00Z",
      role: "builder",
      action: "Created PR",
      duration_ms: 120000,
      outcome: "success",
      issue_number: 42,
      pr_number: 100,
      prompt_preview: "Create pull request for feature X",
      output_preview: "PR #100 created",
      tokens: 3000,
      cost: 0.1,
      event_type: "pr_created",
      label_before: null,
      label_after: "loom:review-requested",
    },
    {
      id: 3,
      timestamp: "2026-01-24T11:00:00Z",
      role: "judge",
      action: "Approved PR",
      duration_ms: 60000,
      outcome: "success",
      issue_number: 42,
      pr_number: 100,
      prompt_preview: "Review PR #100",
      output_preview: "Approved",
      tokens: 2000,
      cost: 0.07,
      event_type: "pr_approved",
      label_before: "loom:review-requested",
      label_after: "loom:pr",
    },
  ];

  beforeEach(async () => {
    // Set up DOM
    document.body.innerHTML = '<div id="app"></div>';

    // Create mock state
    mockState = new AppState();
    mockState.workspace.setWorkspace("/test/workspace");
    setAppState(mockState);

    // Set up invoke mock
    const { invoke } = await import("@tauri-apps/api/core");
    vi.mocked(invoke).mockImplementation(async (cmd: string) => {
      if (cmd === "get_activity_timeline") {
        return mockTimelineEntries;
      }
      if (cmd === "read_recent_activity") {
        return mockTimelineEntries.map((e) => ({
          timestamp: e.timestamp,
          role: e.role,
          trigger: "manual",
          work_found: true,
          work_completed: e.outcome === "success",
          issue_number: e.issue_number,
          duration_ms: e.duration_ms,
          outcome: e.action,
          notes: e.prompt_preview,
        }));
      }
      return [];
    });
  });

  afterEach(() => {
    // Clean up any modal elements
    const modal = document.getElementById("activity-playback-modal");
    if (modal) {
      modal.remove();
    }
    vi.clearAllMocks();
  });

  describe("showActivityPlaybackModal", () => {
    it("should create and show modal", async () => {
      await showActivityPlaybackModal();

      const modal = document.getElementById("activity-playback-modal");
      expect(modal).toBeTruthy();
      expect(modal?.classList.contains("hidden")).toBe(false);
    });

    it("should display timeline entries", async () => {
      await showActivityPlaybackModal();

      // Wait for async data loading
      await new Promise((resolve) => setTimeout(resolve, 100));

      const modal = document.getElementById("activity-playback-modal");
      const content = modal?.innerHTML || "";

      // Check that timeline entries are rendered
      expect(content).toContain("builder");
      expect(content).toContain("judge");
    });

    it("should show filter controls", async () => {
      await showActivityPlaybackModal();

      const issueFilter = document.getElementById("filter-issue");
      const prFilter = document.getElementById("filter-pr");
      const roleFilter = document.getElementById("filter-role");

      expect(issueFilter).toBeTruthy();
      expect(prFilter).toBeTruthy();
      expect(roleFilter).toBeTruthy();
    });

    it("should show export buttons", async () => {
      await showActivityPlaybackModal();

      const exportMdBtn = document.getElementById("export-md-btn");
      const exportJsonBtn = document.getElementById("export-json-btn");

      expect(exportMdBtn).toBeTruthy();
      expect(exportJsonBtn).toBeTruthy();
    });

    it("should show compare button", async () => {
      await showActivityPlaybackModal();

      const compareBtn = document.getElementById("compare-btn");
      expect(compareBtn).toBeTruthy();
    });
  });

  describe("showActivityPlaybackForIssue", () => {
    it("should open modal with issue filter pre-populated", async () => {
      await showActivityPlaybackForIssue(42);

      // Wait for async data loading
      await new Promise((resolve) => setTimeout(resolve, 100));

      const issueFilter = document.getElementById("filter-issue") as HTMLInputElement;
      expect(issueFilter?.value).toBe("42");
    });
  });

  describe("showActivityPlaybackForPR", () => {
    it("should open modal with PR filter pre-populated", async () => {
      await showActivityPlaybackForPR(100);

      // Wait for async data loading
      await new Promise((resolve) => setTimeout(resolve, 100));

      const prFilter = document.getElementById("filter-pr") as HTMLInputElement;
      expect(prFilter?.value).toBe("100");
    });
  });

  describe("Timeline Summary", () => {
    it("should calculate correct summary statistics", async () => {
      await showActivityPlaybackModal();

      // Wait for async data loading
      await new Promise((resolve) => setTimeout(resolve, 100));

      const modal = document.getElementById("activity-playback-modal");
      const content = modal?.innerHTML || "";

      // Check summary cards are present
      expect(content).toContain("Total Prompts");
      expect(content).toContain("Success Rate");
      expect(content).toContain("Active Time");
      expect(content).toContain("Est. Cost");
    });
  });

  describe("Timeline Node Expansion", () => {
    it("should have expandable timeline nodes", async () => {
      await showActivityPlaybackModal();

      // Wait for async data loading
      await new Promise((resolve) => setTimeout(resolve, 100));

      const toggles = document.querySelectorAll(".timeline-toggle");
      expect(toggles.length).toBeGreaterThan(0);
    });
  });

  describe("Empty State", () => {
    it("should show empty state when no data", async () => {
      // Override mock to return empty array
      const { invoke } = await import("@tauri-apps/api/core");
      vi.mocked(invoke).mockResolvedValue([]);

      await showActivityPlaybackModal();

      // Wait for async data loading
      await new Promise((resolve) => setTimeout(resolve, 100));

      const modal = document.getElementById("activity-playback-modal");
      const content = modal?.innerHTML || "";

      expect(content).toContain("No activity found");
    });
  });

  describe("No Workspace", () => {
    it("should show error when no workspace selected", async () => {
      // Clear workspace
      mockState.workspace.clearWorkspace();

      await showActivityPlaybackModal();

      // Wait for async data loading
      await new Promise((resolve) => setTimeout(resolve, 100));

      const modal = document.getElementById("activity-playback-modal");
      const content = modal?.innerHTML || "";

      expect(content).toContain("No workspace selected");
    });
  });
});

describe("Timeline Entry Types", () => {
  it("should have correct structure for TimelineEntry", () => {
    const entry: TimelineEntry = {
      id: 1,
      timestamp: "2026-01-24T10:00:00Z",
      role: "builder",
      action: "Test action",
      duration_ms: 1000,
      outcome: "success",
      issue_number: 1,
      pr_number: null,
      prompt_preview: "Test prompt",
      output_preview: null,
      tokens: 100,
      cost: 0.01,
      event_type: "test",
      label_before: null,
      label_after: null,
    };

    expect(entry.id).toBe(1);
    expect(entry.outcome).toBe("success");
  });

  it("should support all outcome types", () => {
    const outcomes: Array<TimelineEntry["outcome"]> = [
      "success",
      "failure",
      "pending",
      "in_progress",
    ];

    outcomes.forEach((outcome) => {
      const entry: TimelineEntry = {
        id: 1,
        timestamp: "2026-01-24T10:00:00Z",
        role: "builder",
        action: "Test",
        duration_ms: null,
        outcome,
        issue_number: null,
        pr_number: null,
        prompt_preview: null,
        output_preview: null,
        tokens: null,
        cost: null,
        event_type: null,
        label_before: null,
        label_after: null,
      };

      expect(entry.outcome).toBe(outcome);
    });
  });
});
