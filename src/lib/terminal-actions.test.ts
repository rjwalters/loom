import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AppState, TerminalStatus } from "./state";
import {
  handleRunNowClick,
  startRename,
  type TerminalActionDependencies,
} from "./terminal-actions";

// Helper to assert JSON structured log messages
function assertLogMessage(spy: any, expectedMessage: string) {
  const calls = spy.mock.calls;
  const found = calls.some((call: any[]) => {
    try {
      const log = JSON.parse(call[0]);
      return log.message === expectedMessage;
    } catch {
      return false;
    }
  });
  expect(found, `Expected log with message: ${expectedMessage}`).toBe(true);
}

function assertLogContains(spy: any, expectedSubstring: string) {
  const calls = spy.mock.calls;
  const found = calls.some((call: any[]) => {
    try {
      const log = JSON.parse(call[0]);
      return log.message && log.message.includes(expectedSubstring);
    } catch {
      return false;
    }
  });
  expect(found, `Expected log containing: ${expectedSubstring}`).toBe(true);
}
// Mock dynamic import of autonomous-manager
vi.mock("./autonomous-manager", () => ({
  getAutonomousManager: vi.fn(
    () =>
      ({
        runNow: vi.fn().mockResolvedValue(undefined),
      }) as any
  ),
}));

describe("terminal-actions", () => {
  let state: AppState;
  let mockSaveCurrentConfig: ReturnType<typeof vi.fn>;
  let mockRender: ReturnType<typeof vi.fn>;
  let deps: TerminalActionDependencies;
  let alertSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    state = new AppState();
    mockSaveCurrentConfig = vi.fn().mockResolvedValue(undefined);
    mockRender = vi.fn();

    deps = {
      state,
      saveCurrentConfig: mockSaveCurrentConfig,
      render: mockRender,
    };

    alertSpy = vi.spyOn(window, "alert").mockImplementation(() => {});
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

      expect(alertSpy).toHaveBeenCalledWith(
        expect.stringContaining("Failed to run interval prompt")
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
