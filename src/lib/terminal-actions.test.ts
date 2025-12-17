import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppState, TerminalStatus } from "./state";
import {
  handleRunNowClick,
  startRename,
  type TerminalActionDependencies,
} from "./terminal-actions";
import { showToast } from "./toast";

// Mock autonomous manager with stable instance
const mockRunNow = vi.fn().mockResolvedValue(undefined);
const mockAutonomousManager = {
  runNow: mockRunNow,
};

vi.mock("./autonomous-manager", () => ({
  getAutonomousManager: vi.fn(() => mockAutonomousManager),
}));

vi.mock("./toast", () => ({
  showToast: vi.fn(),
}));

describe("terminal-actions", () => {
  let state: AppState;
  let mockSaveCurrentConfig: () => Promise<void>;
  let mockRender: () => void;
  let deps: TerminalActionDependencies;

  beforeEach(() => {
    state = new AppState();
    mockSaveCurrentConfig = vi.fn<() => Promise<void>>().mockResolvedValue(undefined);
    mockRender = vi.fn<() => void>();

    deps = {
      state,
      saveCurrentConfig: mockSaveCurrentConfig,
      render: mockRender,
    };

    vi.clearAllMocks();
  });

  describe("handleRunNowClick", () => {
    it("executes interval prompt for terminal", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Test",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      const { getAutonomousManager } = await import("./autonomous-manager");
      await handleRunNowClick("term-1", { state });

      expect(getAutonomousManager).toHaveBeenCalled();
      const manager = getAutonomousManager();
      expect(manager.runNow).toHaveBeenCalledWith(expect.objectContaining({ id: "term-1" }));
    });

    it("handles terminal not found", async () => {
      await handleRunNowClick("nonexistent", { state });

      const { getAutonomousManager } = await import("./autonomous-manager");
      const manager = getAutonomousManager();
      expect(manager.runNow).not.toHaveBeenCalled();
    });

    it("handles execution error gracefully", async () => {
      state.addTerminal({
        id: "term-1",
        name: "Test",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      const { getAutonomousManager } = await import("./autonomous-manager");
      vi.mocked(getAutonomousManager).mockReturnValue({
        runNow: vi.fn().mockRejectedValue(new Error("Execution failed")),
      } as any);

      await handleRunNowClick("term-1", { state });

      expect(showToast).toHaveBeenCalledWith(
        expect.stringContaining("Failed to run interval prompt"),
        "error"
      );
    });
  });

  describe("startRename", () => {
    let nameElement: HTMLElement;
    let parentElement: HTMLElement;

    beforeEach(() => {
      vi.useFakeTimers();

      state.addTerminal({
        id: "term-1",
        name: "Original Name",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });

      nameElement = document.createElement("span");
      nameElement.textContent = "Original Name";
      nameElement.classList.add("text-sm");

      parentElement = document.createElement("div");
      parentElement.appendChild(nameElement);
      document.body.appendChild(parentElement);
    });

    afterEach(() => {
      vi.useRealTimers();
      document.body.innerHTML = "";
    });

    it("replaces name element with input", () => {
      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input");
      expect(input).not.toBeNull();
      expect(input?.value).toBe("Original Name");
    });

    it("focuses and selects input text", () => {
      const focusSpy = vi.spyOn(HTMLInputElement.prototype, "focus");
      const selectSpy = vi.spyOn(HTMLInputElement.prototype, "select");

      startRename("term-1", nameElement, deps);
      vi.runAllTimers();

      expect(focusSpy).toHaveBeenCalled();
      expect(selectSpy).toHaveBeenCalled();
    });

    it("applies correct CSS classes based on original element", () => {
      nameElement.classList.add("text-xs");
      nameElement.classList.remove("text-sm");

      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input");
      expect(input?.className).toContain("text-xs");
    });

    it("commits rename on Enter key", () => {
      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input") as HTMLInputElement;
      input.value = "New Name";

      const enterEvent = new KeyboardEvent("keydown", { key: "Enter" });
      input.dispatchEvent(enterEvent);

      const terminal = state.getTerminal("term-1");
      expect(terminal?.name).toBe("New Name");
      expect(mockSaveCurrentConfig).toHaveBeenCalled();
    });

    it("commits rename on blur", () => {
      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input") as HTMLInputElement;
      input.value = "New Name via Blur";

      input.dispatchEvent(new Event("blur"));

      const terminal = state.getTerminal("term-1");
      expect(terminal?.name).toBe("New Name via Blur");
      expect(mockSaveCurrentConfig).toHaveBeenCalled();
    });

    it("cancels rename on Escape key", () => {
      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input") as HTMLInputElement;
      input.value = "Changed";

      const escapeEvent = new KeyboardEvent("keydown", { key: "Escape" });
      input.dispatchEvent(escapeEvent);

      const terminal = state.getTerminal("term-1");
      expect(terminal?.name).toBe("Original Name");
      expect(mockRender).toHaveBeenCalled();
      expect(mockSaveCurrentConfig).not.toHaveBeenCalled();
    });

    it("does not rename when value is empty", () => {
      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input") as HTMLInputElement;
      input.value = "   ";

      input.dispatchEvent(new Event("blur"));

      const terminal = state.getTerminal("term-1");
      expect(terminal?.name).toBe("Original Name");
      expect(mockRender).toHaveBeenCalled();
      expect(mockSaveCurrentConfig).not.toHaveBeenCalled();
    });

    it("does not rename when value is unchanged", () => {
      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input") as HTMLInputElement;
      // Value is already "Original Name"

      input.dispatchEvent(new Event("blur"));

      expect(mockRender).toHaveBeenCalled();
      expect(mockSaveCurrentConfig).not.toHaveBeenCalled();
    });

    it("trims whitespace from new name", () => {
      startRename("term-1", nameElement, deps);

      const input = parentElement.querySelector("input") as HTMLInputElement;
      input.value = "  Trimmed Name  ";

      input.dispatchEvent(new Event("blur"));

      const terminal = state.getTerminal("term-1");
      expect(terminal?.name).toBe("Trimmed Name");
    });

    it("handles missing terminal gracefully", () => {
      startRename("nonexistent", nameElement, deps);

      const input = parentElement.querySelector("input");
      expect(input).toBeNull();
    });

    it("handles missing parent element gracefully", () => {
      const orphanElement = document.createElement("span");
      orphanElement.textContent = "Orphan";

      expect(() => startRename("term-1", orphanElement, deps)).not.toThrow();
    });
  });
});
