import { beforeEach, describe, expect, it, vi } from "vitest";
import { clearWorkspaceError, expandTildePath, showWorkspaceError } from "./workspace-utils";

// Mock the Tauri API
vi.mock("@tauri-apps/api/path", () => ({
  homeDir: vi.fn(),
}));

import { homeDir } from "@tauri-apps/api/path";

describe("workspace-utils", () => {
  beforeEach(() => {
    // Reset mocks
    vi.clearAllMocks();

    // Reset DOM
    document.body.innerHTML = `
      <input id="workspace-path" class="border-gray-300 dark:border-gray-600" />
      <div id="workspace-error"></div>
    `;
  });

  describe("expandTildePath", () => {
    it("expands tilde to home directory", async () => {
      vi.mocked(homeDir).mockResolvedValue("/Users/testuser");

      const result = await expandTildePath("~/Documents");

      expect(result).toBe("/Users/testuser/Documents");
    });

    it("expands tilde with slash-only path", async () => {
      vi.mocked(homeDir).mockResolvedValue("/Users/testuser");

      const result = await expandTildePath("~");

      expect(result).toBe("/Users/testuser");
    });

    it("expands tilde with nested path", async () => {
      vi.mocked(homeDir).mockResolvedValue("/Users/testuser");

      const result = await expandTildePath("~/Projects/my-app/src");

      expect(result).toBe("/Users/testuser/Projects/my-app/src");
    });

    it("returns absolute path unchanged", async () => {
      const result = await expandTildePath("/absolute/path");

      expect(result).toBe("/absolute/path");
      expect(homeDir).not.toHaveBeenCalled();
    });

    it("returns relative path unchanged", async () => {
      const result = await expandTildePath("relative/path");

      expect(result).toBe("relative/path");
      expect(homeDir).not.toHaveBeenCalled();
    });

    it("handles homeDir error gracefully", async () => {
      vi.mocked(homeDir).mockRejectedValue(new Error("Cannot get home dir"));

      const result = await expandTildePath("~/Documents");

      expect(result).toBe("~/Documents");
    });

    it("handles empty string", async () => {
      const result = await expandTildePath("");

      expect(result).toBe("");
      expect(homeDir).not.toHaveBeenCalled();
    });

    it("does not expand tilde in middle of path", async () => {
      const result = await expandTildePath("/path/~/middle");

      expect(result).toBe("/path/~/middle");
      expect(homeDir).not.toHaveBeenCalled();
    });
  });

  describe("showWorkspaceError", () => {
    it("displays error message", () => {
      const errorDiv = document.getElementById("workspace-error");

      showWorkspaceError("Invalid workspace path");

      expect(errorDiv?.textContent).toBe("Invalid workspace path");
    });

    it("adds error styling to input", () => {
      const input = document.getElementById("workspace-path") as HTMLInputElement;

      showWorkspaceError("Error message");

      expect(input.classList.contains("border-red-500")).toBe(true);
      expect(input.classList.contains("dark:border-red-500")).toBe(true);
      expect(input.classList.contains("border-gray-300")).toBe(false);
      expect(input.classList.contains("dark:border-gray-600")).toBe(false);
    });

    it("handles missing input element gracefully", () => {
      document.getElementById("workspace-path")?.remove();

      expect(() => showWorkspaceError("Error")).not.toThrow();
    });

    it("handles missing error div gracefully", () => {
      document.getElementById("workspace-error")?.remove();

      expect(() => showWorkspaceError("Error")).not.toThrow();
    });

    it("updates error message when called multiple times", () => {
      const errorDiv = document.getElementById("workspace-error");

      showWorkspaceError("First error");
      expect(errorDiv?.textContent).toBe("First error");

      showWorkspaceError("Second error");
      expect(errorDiv?.textContent).toBe("Second error");
    });
  });

  describe("clearWorkspaceError", () => {
    it("clears error message", () => {
      const errorDiv = document.getElementById("workspace-error");
      if (errorDiv) errorDiv.textContent = "Error message";

      clearWorkspaceError();

      expect(errorDiv?.textContent).toBe("");
    });

    it("removes error styling from input", () => {
      const input = document.getElementById("workspace-path") as HTMLInputElement;
      input.classList.remove("border-gray-300", "dark:border-gray-600");
      input.classList.add("border-red-500", "dark:border-red-500");

      clearWorkspaceError();

      expect(input.classList.contains("border-red-500")).toBe(false);
      expect(input.classList.contains("dark:border-red-500")).toBe(false);
      expect(input.classList.contains("border-gray-300")).toBe(true);
      expect(input.classList.contains("dark:border-gray-600")).toBe(true);
    });

    it("handles missing input element gracefully", () => {
      document.getElementById("workspace-path")?.remove();

      expect(() => clearWorkspaceError()).not.toThrow();
    });

    it("handles missing error div gracefully", () => {
      document.getElementById("workspace-error")?.remove();

      expect(() => clearWorkspaceError()).not.toThrow();
    });
  });

  describe("integration", () => {
    it("show and clear error work together", () => {
      const input = document.getElementById("workspace-path") as HTMLInputElement;
      const errorDiv = document.getElementById("workspace-error");

      // Show error
      showWorkspaceError("Test error");
      expect(errorDiv?.textContent).toBe("Test error");
      expect(input.classList.contains("border-red-500")).toBe(true);

      // Clear error
      clearWorkspaceError();
      expect(errorDiv?.textContent).toBe("");
      expect(input.classList.contains("border-red-500")).toBe(false);
      expect(input.classList.contains("border-gray-300")).toBe(true);
    });

    it("can show multiple errors and clear each time", () => {
      const errorDiv = document.getElementById("workspace-error");

      showWorkspaceError("Error 1");
      expect(errorDiv?.textContent).toBe("Error 1");

      clearWorkspaceError();
      expect(errorDiv?.textContent).toBe("");

      showWorkspaceError("Error 2");
      expect(errorDiv?.textContent).toBe("Error 2");

      clearWorkspaceError();
      expect(errorDiv?.textContent).toBe("");
    });
  });
});
