import { beforeEach, describe, expect, it, vi } from "vitest";
import { checkAndReportDependencies, getAvailableWorkerTypes } from "./dependency-checker";

// Mock Tauri APIs
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/dialog", () => ({
  ask: vi.fn(),
}));

import { ask } from "@tauri-apps/api/dialog";
import { invoke } from "@tauri-apps/api/tauri";

describe("dependency-checker", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  describe("checkAndReportDependencies", () => {
    it("returns true when all critical deps and at least one agent available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: true,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const result = await checkAndReportDependencies();

      expect(result).toBe(true);
      expect(ask).not.toHaveBeenCalled();
    });

    it("shows dialog when tmux is missing", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: false,
        git_available: true,
        claude_code_available: true,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      vi.mocked(ask).mockResolvedValue(false);

      const result = await checkAndReportDependencies();

      expect(ask).toHaveBeenCalledWith(expect.stringContaining("tmux"), expect.anything());
      expect(result).toBe(false);
    });

    it("shows dialog when git is missing", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: false,
        claude_code_available: true,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      vi.mocked(ask).mockResolvedValue(false);

      const result = await checkAndReportDependencies();

      expect(ask).toHaveBeenCalledWith(expect.stringContaining("git"), expect.anything());
      expect(result).toBe(false);
    });

    it("shows dialog when no agent is available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      vi.mocked(ask).mockResolvedValue(false);

      const result = await checkAndReportDependencies();

      expect(ask).toHaveBeenCalledWith(
        expect.stringContaining("AI coding agent"),
        expect.anything()
      );
      expect(result).toBe(false);
    });

    it("accepts GitHub Copilot as valid agent", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: true,
        gh_copilot_available: true,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const result = await checkAndReportDependencies();

      expect(result).toBe(true);
      expect(ask).not.toHaveBeenCalled();
    });

    it("accepts Gemini CLI as valid agent", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: true,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const result = await checkAndReportDependencies();

      expect(result).toBe(true);
    });

    it("retries check when user clicks retry", async () => {
      // First call: missing Claude
      vi.mocked(invoke)
        .mockResolvedValueOnce({
          tmux_available: true,
          git_available: true,
          claude_code_available: false,
          gh_available: false,
          gh_copilot_available: false,
          gemini_cli_available: false,
          deepseek_cli_available: false,
          grok_cli_available: false,
        })
        // Second call: Claude installed
        .mockResolvedValueOnce({
          tmux_available: true,
          git_available: true,
          claude_code_available: true,
          gh_available: false,
          gh_copilot_available: false,
          gemini_cli_available: false,
          deepseek_cli_available: false,
          grok_cli_available: false,
        });

      vi.mocked(ask).mockResolvedValue(true);

      const result = await checkAndReportDependencies();

      expect(ask).toHaveBeenCalledOnce();
      expect(invoke).toHaveBeenCalledTimes(2);
      expect(result).toBe(true);
    });

    it("returns false when user declines retry", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: false,
        git_available: true,
        claude_code_available: true,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      vi.mocked(ask).mockResolvedValue(false);

      const result = await checkAndReportDependencies();

      expect(result).toBe(false);
    });
  });

  describe("getAvailableWorkerTypes", () => {
    it("returns Claude when available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: true,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const types = await getAvailableWorkerTypes();

      expect(types).toContainEqual({ value: "claude", label: "Claude Code" });
    });

    it("returns GitHub Copilot when both gh and extension available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: true,
        gh_copilot_available: true,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const types = await getAvailableWorkerTypes();

      expect(types).toContainEqual({ value: "github-copilot", label: "GitHub Copilot" });
    });

    it("does not return GitHub Copilot when extension missing", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: true,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const types = await getAvailableWorkerTypes();

      const hasCopilot = types.some((t) => t.value === "github-copilot");
      expect(hasCopilot).toBe(false);
    });

    it("always includes Codex as available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const types = await getAvailableWorkerTypes();

      expect(types).toContainEqual({ value: "codex", label: "Codex" });
    });

    it("returns Gemini when available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: true,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const types = await getAvailableWorkerTypes();

      expect(types).toContainEqual({ value: "gemini", label: "Google Gemini" });
    });

    it("returns DeepSeek when available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: true,
        grok_cli_available: false,
      });

      const types = await getAvailableWorkerTypes();

      expect(types).toContainEqual({ value: "deepseek", label: "DeepSeek Coder" });
    });

    it("returns Grok when available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: false,
        gh_available: false,
        gh_copilot_available: false,
        gemini_cli_available: false,
        deepseek_cli_available: false,
        grok_cli_available: true,
      });

      const types = await getAvailableWorkerTypes();

      expect(types).toContainEqual({ value: "grok", label: "xAI Grok" });
    });

    it("returns multiple worker types when multiple agents available", async () => {
      vi.mocked(invoke).mockResolvedValue({
        tmux_available: true,
        git_available: true,
        claude_code_available: true,
        gh_available: true,
        gh_copilot_available: true,
        gemini_cli_available: true,
        deepseek_cli_available: false,
        grok_cli_available: false,
      });

      const types = await getAvailableWorkerTypes();

      expect(types.length).toBeGreaterThanOrEqual(4); // Claude, Codex, GitHub Copilot, Gemini
      expect(types).toContainEqual({ value: "claude", label: "Claude Code" });
      expect(types).toContainEqual({ value: "codex", label: "Codex" });
      expect(types).toContainEqual({ value: "github-copilot", label: "GitHub Copilot" });
      expect(types).toContainEqual({ value: "gemini", label: "Google Gemini" });
    });
  });
});
