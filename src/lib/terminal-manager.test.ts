import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ManagedTerminal } from "./terminal-manager";
import { getTerminalManager, TerminalManager } from "./terminal-manager";

// Mock xterm.js
const mockTerminalInstance = {
  open: vi.fn(),
  write: vi.fn(),
  clear: vi.fn(),
  dispose: vi.fn(),
  onData: vi.fn((callback: (data: string) => void) => {
    // Store callback for testing
    (mockTerminalInstance as any)._dataCallback = callback;
    return { dispose: vi.fn() };
  }),
  onBell: vi.fn((callback: () => void) => {
    // Store callback for testing
    (mockTerminalInstance as any)._bellCallback = callback;
    return { dispose: vi.fn() };
  }),
  loadAddon: vi.fn(),
  options: {
    fontSize: 14,
    theme: {},
  },
};

vi.mock("@xterm/xterm", () => ({
  Terminal: vi.fn(() => mockTerminalInstance),
}));

vi.mock("@xterm/addon-fit", () => ({
  FitAddon: vi.fn(() => ({
    fit: vi.fn(),
    proposeDimensions: vi.fn(() => ({ cols: 80, rows: 24 })),
  })),
}));

vi.mock("@xterm/addon-web-links", () => ({
  WebLinksAddon: vi.fn(() => ({})),
}));

// Create a mock that can throw on demand
let webglShouldThrow = false;
vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: vi.fn(() => {
    if (webglShouldThrow) {
      throw new Error("WebGL not supported");
    }
    return {};
  }),
}));

// Mock Tauri API
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

// Mock state module
vi.mock("./state", () => ({
  getAppState: vi.fn(),
  TerminalStatus: {
    Idle: "idle",
    Busy: "busy",
    NeedsInput: "needs_input",
    Error: "error",
    Stopped: "stopped",
  },
}));

import { invoke } from "@tauri-apps/api/tauri";
import { Terminal } from "@xterm/xterm";
import { getAppState, TerminalStatus } from "./state";

describe("TerminalManager", () => {
  let manager: TerminalManager;
  let consoleLogSpy: ReturnType<typeof vi.spyOn>;
  let consoleWarnSpy: ReturnType<typeof vi.spyOn>;
  let consoleErrorSpy: ReturnType<typeof vi.spyOn>;
  let persistentContainer: HTMLElement;

  // Mock state
  const mockTerminal = {
    id: "terminal-1",
    name: "Test Terminal",
    status: TerminalStatus.NeedsInput,
  };

  const mockState = {
    getTerminal: vi.fn(() => mockTerminal),
    updateTerminal: vi.fn(),
  };

  // Mock localStorage
  const localStorageMock = (() => {
    let store: Record<string, string> = {};
    return {
      getItem: (key: string) => store[key] || null,
      setItem: (key: string, value: string) => {
        store[key] = value;
      },
      removeItem: (key: string) => {
        delete store[key];
      },
      clear: () => {
        store = {};
      },
    };
  })();

  beforeEach(() => {
    // Reset mocks
    vi.clearAllMocks();

    // Setup console spies
    consoleLogSpy = vi.spyOn(console, "log").mockImplementation(() => {});
    consoleWarnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
    consoleErrorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    // Setup localStorage mock
    Object.defineProperty(window, "localStorage", {
      value: localStorageMock,
      writable: true,
    });
    localStorageMock.clear();

    // Setup DOM
    document.body.innerHTML = '<div id="persistent-xterm-containers"></div>';
    persistentContainer = document.getElementById("persistent-xterm-containers")!;

    // Setup mock implementations
    vi.mocked(getAppState).mockReturnValue(mockState as any);
    vi.mocked(invoke).mockResolvedValue(undefined);

    // Reset Terminal mock
    (mockTerminalInstance as any)._dataCallback = null;
    (mockTerminalInstance as any)._bellCallback = null;
    mockTerminalInstance.write.mockClear();
    mockTerminalInstance.clear.mockClear();
    mockTerminalInstance.open.mockClear();
    mockTerminalInstance.dispose.mockClear();
    mockTerminalInstance.onData.mockClear();
    mockTerminalInstance.onBell.mockClear();
    mockTerminalInstance.loadAddon.mockClear();
    mockTerminalInstance.options.fontSize = 14;

    // Create fresh manager instance
    manager = new TerminalManager();
  });

  afterEach(() => {
    manager.destroyAll();
    consoleLogSpy.mockRestore();
    consoleWarnSpy.mockRestore();
    consoleErrorSpy.mockRestore();
    document.body.innerHTML = "";
  });

  describe("Terminal Creation", () => {
    it("creates a new terminal instance", () => {
      const managed = manager.createTerminal("terminal-1", "container-1");

      expect(managed).not.toBeNull();
      expect(managed?.terminal).toBeDefined();
      expect(managed?.fitAddon).toBeDefined();
      expect(managed?.container).toBeDefined();
      expect(managed?.attached).toBe(false);
    });

    it("creates terminal with correct configuration", () => {
      manager.createTerminal("terminal-1", "container-1");

      expect(Terminal).toHaveBeenCalledWith(
        expect.objectContaining({
          cols: 80,
          rows: 24,
          cursorBlink: true,
          fontSize: 14,
          fontFamily: 'Menlo, Monaco, "Courier New", monospace',
          scrollback: 10000,
        })
      );
    });

    it("creates container element in persistent area", () => {
      manager.createTerminal("terminal-1", "container-1");

      const container = document.getElementById("xterm-container-terminal-1");
      expect(container).not.toBeNull();
      expect(container?.parentElement).toBe(persistentContainer);
      expect(container?.style.display).toBe("none"); // Hidden by default
    });

    it("opens terminal in container", () => {
      manager.createTerminal("terminal-1", "container-1");

      expect(mockTerminalInstance.open).toHaveBeenCalled();
    });

    it("loads terminal addons", () => {
      manager.createTerminal("terminal-1", "container-1");

      // Should load FitAddon, WebLinksAddon, and attempt WebglAddon
      expect(mockTerminalInstance.loadAddon).toHaveBeenCalledTimes(3);
    });

    it("handles WebGL addon failure gracefully", () => {
      // Set flag to make WebGL throw
      webglShouldThrow = true;

      manager.createTerminal("terminal-1", "container-1");

      expect(consoleWarnSpy).toHaveBeenCalledWith(
        expect.stringContaining("WebGL addon failed to load"),
        expect.any(Error)
      );

      // Reset flag
      webglShouldThrow = false;
    });

    it("prevents creating duplicate terminals", () => {
      manager.createTerminal("terminal-1", "container-1");
      const second = manager.createTerminal("terminal-1", "container-1");

      expect(consoleWarnSpy).toHaveBeenCalledWith("Terminal terminal-1 already exists");
      expect(second).not.toBeNull(); // Returns existing
    });

    it("returns null if persistent container not found", () => {
      document.body.innerHTML = ""; // Remove persistent container

      const managed = manager.createTerminal("terminal-1", "container-1");

      expect(managed).toBeNull();
      expect(consoleErrorSpy).toHaveBeenCalledWith(
        "persistent-xterm-containers not found - UI not initialized"
      );
    });

    it("uses saved font size from localStorage", () => {
      localStorage.setItem("terminal-font-size", "18");

      manager.createTerminal("terminal-1", "container-1");

      expect(Terminal).toHaveBeenCalledWith(
        expect.objectContaining({
          fontSize: 18,
        })
      );
    });

    it("uses default font size if localStorage has invalid value", () => {
      localStorage.setItem("terminal-font-size", "invalid");

      manager.createTerminal("terminal-1", "container-1");

      expect(Terminal).toHaveBeenCalledWith(
        expect.objectContaining({
          fontSize: 14,
        })
      );
    });
  });

  describe("Terminal Input Handling", () => {
    it("sends terminal input to daemon via IPC", async () => {
      manager.createTerminal("terminal-1", "container-1");

      const dataCallback = (mockTerminalInstance as any)._dataCallback;
      expect(dataCallback).toBeDefined();

      // Simulate user typing
      await dataCallback("ls\r");

      // Need to wait for dynamic import
      await vi.waitFor(() => {
        expect(invoke).toHaveBeenCalledWith("send_terminal_input", {
          id: "terminal-1",
          data: "ls\r",
        });
      });
    });

    it("clears needs-input state when user types", async () => {
      manager.createTerminal("terminal-1", "container-1");

      const dataCallback = (mockTerminalInstance as any)._dataCallback;

      // Simulate user typing
      await dataCallback("test");

      // Need to wait for dynamic import
      await vi.waitFor(() => {
        expect(mockState.updateTerminal).toHaveBeenCalledWith("terminal-1", {
          status: TerminalStatus.Idle,
        });
      });
    });

    it("handles IPC send input errors gracefully", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("IPC error"));

      manager.createTerminal("terminal-1", "container-1");

      const dataCallback = (mockTerminalInstance as any)._dataCallback;
      await dataCallback("test");

      await vi.waitFor(() => {
        expect(consoleErrorSpy).toHaveBeenCalledWith(
          expect.stringContaining("Failed to send input for terminal-1"),
          expect.any(Error)
        );
      });
    });
  });

  describe("Terminal Bell Handling", () => {
    it("sets needs-input state on bell", async () => {
      manager.createTerminal("terminal-1", "container-1");

      const bellCallback = (mockTerminalInstance as any)._bellCallback;
      expect(bellCallback).toBeDefined();

      // Simulate bell
      await bellCallback();

      // Need to wait for dynamic import
      await vi.waitFor(() => {
        expect(mockState.updateTerminal).toHaveBeenCalledWith("terminal-1", {
          status: TerminalStatus.NeedsInput,
        });
      });
    });
  });

  describe("Terminal Retrieval", () => {
    it("gets terminal by ID", () => {
      manager.createTerminal("terminal-1", "container-1");

      const managed = manager.getTerminal("terminal-1");

      expect(managed).toBeDefined();
      expect(managed?.terminal).toBe(mockTerminalInstance);
    });

    it("returns undefined for non-existent terminal", () => {
      const managed = manager.getTerminal("non-existent");

      expect(managed).toBeUndefined();
    });

    it("gets all terminal IDs", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      const ids = manager.getTerminalIds();

      expect(ids).toContain("terminal-1");
      expect(ids).toContain("terminal-2");
      expect(ids.length).toBe(2);
    });

    it("gets terminal count", () => {
      expect(manager.getTerminalCount()).toBe(0);

      manager.createTerminal("terminal-1", "container-1");
      expect(manager.getTerminalCount()).toBe(1);

      manager.createTerminal("terminal-2", "container-2");
      expect(manager.getTerminalCount()).toBe(2);

      manager.destroyTerminal("terminal-1");
      expect(manager.getTerminalCount()).toBe(1);
    });
  });

  describe("Terminal Visibility", () => {
    it("shows terminal by updating display style", () => {
      manager.createTerminal("terminal-1", "container-1");
      const container = document.getElementById("xterm-container-terminal-1")!;

      manager.showTerminal("terminal-1");

      expect(container.style.display).toBe("block");
      expect(consoleLogSpy).toHaveBeenCalledWith("[terminal-manager] Showing terminal terminal-1");
    });

    it("hides terminal by updating display style", () => {
      manager.createTerminal("terminal-1", "container-1");
      const container = document.getElementById("xterm-container-terminal-1")!;

      manager.showTerminal("terminal-1");
      manager.hideTerminal("terminal-1");

      expect(container.style.display).toBe("none");
      expect(consoleLogSpy).toHaveBeenCalledWith("[terminal-manager] Hiding terminal terminal-1");
    });

    it("warns when showing non-existent terminal", () => {
      manager.showTerminal("non-existent");

      expect(consoleWarnSpy).toHaveBeenCalledWith("Terminal non-existent not found");
    });

    it("warns when hiding non-existent terminal", () => {
      manager.hideTerminal("non-existent");

      expect(consoleWarnSpy).toHaveBeenCalledWith("Terminal non-existent not found");
    });

    it("hides all terminals", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      manager.showTerminal("terminal-1");
      manager.showTerminal("terminal-2");

      manager.hideAllTerminals();

      const container1 = document.getElementById("xterm-container-terminal-1")!;
      const container2 = document.getElementById("xterm-container-terminal-2")!;

      expect(container1.style.display).toBe("none");
      expect(container2.style.display).toBe("none");
    });
  });

  describe("Terminal Writing", () => {
    it("writes data to terminal", () => {
      manager.createTerminal("terminal-1", "container-1");

      manager.writeToTerminal("terminal-1", "Hello, World!");

      expect(mockTerminalInstance.write).toHaveBeenCalledWith("Hello, World!");
    });

    it("warns when writing to non-existent terminal", () => {
      manager.writeToTerminal("non-existent", "test");

      expect(consoleWarnSpy).toHaveBeenCalledWith("Terminal non-existent not found");
      expect(mockTerminalInstance.write).not.toHaveBeenCalled();
    });

    it("clears and writes terminal", () => {
      manager.createTerminal("terminal-1", "container-1");

      manager.clearAndWriteTerminal("terminal-1", "Fresh content");

      expect(mockTerminalInstance.clear).toHaveBeenCalled();
      expect(mockTerminalInstance.write).toHaveBeenCalledWith("\x1b[H"); // Reset cursor
      expect(mockTerminalInstance.write).toHaveBeenCalledWith("Fresh content");
    });

    it("warns when clearing and writing to non-existent terminal", () => {
      manager.clearAndWriteTerminal("non-existent", "test");

      expect(consoleWarnSpy).toHaveBeenCalledWith("Terminal non-existent not found");
      expect(mockTerminalInstance.clear).not.toHaveBeenCalled();
    });

    it("clears terminal", () => {
      manager.createTerminal("terminal-1", "container-1");

      manager.clearTerminal("terminal-1");

      expect(mockTerminalInstance.clear).toHaveBeenCalled();
    });

    it("warns when clearing non-existent terminal", () => {
      manager.clearTerminal("non-existent");

      expect(consoleWarnSpy).toHaveBeenCalledWith("Terminal non-existent not found");
    });
  });

  describe("Terminal Destruction", () => {
    it("destroys terminal and cleans up resources", () => {
      manager.createTerminal("terminal-1", "container-1");

      manager.destroyTerminal("terminal-1");

      expect(mockTerminalInstance.dispose).toHaveBeenCalled();
      expect(manager.getTerminal("terminal-1")).toBeUndefined();
    });

    it("handles destroying non-existent terminal gracefully", () => {
      manager.destroyTerminal("non-existent");

      // Should not throw or log error
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });

    it("destroys all terminals", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      manager.destroyAll();

      expect(mockTerminalInstance.dispose).toHaveBeenCalledTimes(2);
      expect(manager.getTerminalCount()).toBe(0);
    });
  });

  describe("Terminal Attachment", () => {
    it("marks terminal as attached", () => {
      manager.createTerminal("terminal-1", "container-1");

      expect(manager.isAttached("terminal-1")).toBe(false);

      manager.markAttached("terminal-1", true);

      expect(manager.isAttached("terminal-1")).toBe(true);
    });

    it("marks terminal as detached", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.markAttached("terminal-1", true);

      manager.markAttached("terminal-1", false);

      expect(manager.isAttached("terminal-1")).toBe(false);
    });

    it("returns false for non-existent terminal attachment", () => {
      expect(manager.isAttached("non-existent")).toBe(false);
    });

    it("handles marking non-existent terminal gracefully", () => {
      manager.markAttached("non-existent", true);

      // Should not throw
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });
  });

  describe("Theme Management", () => {
    it("updates terminal theme to dark mode", () => {
      manager.createTerminal("terminal-1", "container-1");

      manager.updateTheme("terminal-1", true);

      expect(mockTerminalInstance.options.theme).toMatchObject({
        background: "#1e1e1e",
        foreground: "#d4d4d4",
        cursor: "#ffffff",
      });
    });

    it("updates terminal theme to light mode", () => {
      manager.createTerminal("terminal-1", "container-1");

      manager.updateTheme("terminal-1", false);

      expect(mockTerminalInstance.options.theme).toMatchObject({
        background: "#ffffff",
        foreground: "#333333",
        cursor: "#000000",
      });
    });

    it("updates all terminals themes", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      // Mock to track theme updates
      const themeSpy = vi.fn();
      Object.defineProperty(mockTerminalInstance.options, "theme", {
        set: themeSpy,
        get: () => ({}),
        configurable: true,
      });

      manager.updateAllThemes(true);

      expect(themeSpy).toHaveBeenCalledTimes(2);
    });

    it("handles updating theme for non-existent terminal gracefully", () => {
      manager.updateTheme("non-existent", true);

      // Should not throw
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });
  });

  describe("Font Size Management", () => {
    it("adjusts font size for terminal", () => {
      manager.createTerminal("terminal-1", "container-1");

      manager.adjustFontSize("terminal-1", 2);

      expect(mockTerminalInstance.options.fontSize).toBe(16);
      expect(localStorage.getItem("terminal-font-size")).toBe("16");
    });

    it("clamps font size to minimum", () => {
      manager.createTerminal("terminal-1", "container-1");
      mockTerminalInstance.options.fontSize = 8;

      manager.adjustFontSize("terminal-1", -5);

      expect(mockTerminalInstance.options.fontSize).toBe(8); // Min is 8
    });

    it("clamps font size to maximum", () => {
      manager.createTerminal("terminal-1", "container-1");
      mockTerminalInstance.options.fontSize = 32;

      manager.adjustFontSize("terminal-1", 5);

      expect(mockTerminalInstance.options.fontSize).toBe(32); // Max is 32
    });

    it("adjusts font size for all terminals", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      manager.adjustAllFontSizes(4);

      // Both terminals should have new size
      const terminal1 = manager.getTerminal("terminal-1");
      const terminal2 = manager.getTerminal("terminal-2");

      expect(terminal1?.terminal.options.fontSize).toBe(18);
      expect(terminal2?.terminal.options.fontSize).toBe(18);
      expect(localStorage.getItem("terminal-font-size")).toBe("18");
    });

    it("handles adjusting font size when no terminals exist", () => {
      manager.adjustAllFontSizes(2);

      // Should not throw
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });

    it("resets all font sizes to default", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      manager.adjustAllFontSizes(6); // Set to 20
      manager.resetAllFontSizes();

      const terminal1 = manager.getTerminal("terminal-1");
      const terminal2 = manager.getTerminal("terminal-2");

      expect(terminal1?.terminal.options.fontSize).toBe(14);
      expect(terminal2?.terminal.options.fontSize).toBe(14);
      expect(localStorage.getItem("terminal-font-size")).toBeNull();
    });

    it("handles adjusting font size for non-existent terminal gracefully", () => {
      manager.adjustFontSize("non-existent", 2);

      // Should not throw
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });

    it("gets saved font size from localStorage", () => {
      localStorage.setItem("terminal-font-size", "20");

      const fontSize = manager.getSavedFontSize();

      expect(fontSize).toBe(20);
    });

    it("returns default font size if localStorage is empty", () => {
      const fontSize = manager.getSavedFontSize();

      expect(fontSize).toBe(14);
    });

    it("returns default font size if localStorage has invalid value", () => {
      localStorage.setItem("terminal-font-size", "invalid");

      const fontSize = manager.getSavedFontSize();

      expect(fontSize).toBe(14);
    });

    it("validates font size range from localStorage", () => {
      localStorage.setItem("terminal-font-size", "50"); // Out of range

      const fontSize = manager.getSavedFontSize();

      expect(fontSize).toBe(14); // Falls back to default
    });
  });

  describe("Terminal Fit (No-op)", () => {
    it("fitTerminal is a no-op", async () => {
      manager.createTerminal("terminal-1", "container-1");

      await manager.fitTerminal("terminal-1");

      expect(consoleLogSpy).toHaveBeenCalledWith(
        expect.stringContaining("Skipping resize for terminal-1 (using fixed size)")
      );
    });

    it("fitAllTerminals is a no-op", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      manager.fitAllTerminals();

      // Should not throw, just no-op
      expect(consoleErrorSpy).not.toHaveBeenCalled();
    });
  });

  describe("Singleton Instance", () => {
    it("returns same instance from getTerminalManager", () => {
      const instance1 = getTerminalManager();
      const instance2 = getTerminalManager();

      expect(instance1).toBe(instance2);
    });
  });

  describe("Real-world Scenarios", () => {
    it("creates, shows, writes, hides, and destroys terminal", () => {
      const managed = manager.createTerminal("terminal-1", "container-1");
      expect(managed).not.toBeNull();

      manager.showTerminal("terminal-1");
      const container = document.getElementById("xterm-container-terminal-1")!;
      expect(container.style.display).toBe("block");

      manager.writeToTerminal("terminal-1", "$ ls\n");
      expect(mockTerminalInstance.write).toHaveBeenCalledWith("$ ls\n");

      manager.hideTerminal("terminal-1");
      expect(container.style.display).toBe("none");

      manager.destroyTerminal("terminal-1");
      expect(mockTerminalInstance.dispose).toHaveBeenCalled();
    });

    it("manages multiple terminals independently", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      manager.showTerminal("terminal-1");
      manager.hideTerminal("terminal-2");

      const container1 = document.getElementById("xterm-container-terminal-1")!;
      const container2 = document.getElementById("xterm-container-terminal-2")!;

      expect(container1.style.display).toBe("block");
      expect(container2.style.display).toBe("none");
    });

    it("switches between terminals", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      // Show first
      manager.showTerminal("terminal-1");
      let container1 = document.getElementById("xterm-container-terminal-1")!;
      let container2 = document.getElementById("xterm-container-terminal-2")!;
      expect(container1.style.display).toBe("block");

      // Switch to second
      manager.hideTerminal("terminal-1");
      manager.showTerminal("terminal-2");
      container1 = document.getElementById("xterm-container-terminal-1")!;
      container2 = document.getElementById("xterm-container-terminal-2")!;
      expect(container1.style.display).toBe("none");
      expect(container2.style.display).toBe("block");
    });

    it("adjusts font size globally and persists", () => {
      manager.createTerminal("terminal-1", "container-1");
      manager.createTerminal("terminal-2", "container-2");

      // Increase font size
      manager.adjustAllFontSizes(4);

      // Create new terminal - should use saved size
      mockTerminalInstance.options.fontSize = 14; // Reset mock
      manager.createTerminal("terminal-3", "container-3");

      expect(Terminal).toHaveBeenLastCalledWith(
        expect.objectContaining({
          fontSize: 18, // Saved size from previous adjustment
        })
      );
    });

    it("handles rapid terminal creation and destruction", () => {
      for (let i = 1; i <= 5; i++) {
        manager.createTerminal(`terminal-${i}`, `container-${i}`);
      }

      expect(manager.getTerminalCount()).toBe(5);

      for (let i = 1; i <= 5; i++) {
        manager.destroyTerminal(`terminal-${i}`);
      }

      expect(manager.getTerminalCount()).toBe(0);
    });
  });
});
