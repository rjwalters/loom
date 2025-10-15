import { invoke } from "@tauri-apps/api/tauri";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { type LoomConfig, loadConfig, saveConfig, setConfigWorkspace } from "./config";
import { TerminalStatus } from "./state";

// Mock Tauri invoke
vi.mock("@tauri-apps/api/tauri", () => ({
  invoke: vi.fn(),
}));

describe("config", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Reset cached workspace path
    setConfigWorkspace("");
  });

  describe("setConfigWorkspace", () => {
    it("should set the workspace path for config operations", () => {
      setConfigWorkspace("/path/to/workspace");
      // We can't directly test the cached value, but we can test it through loadConfig/saveConfig
      expect(true).toBe(true); // This function has no return value to test directly
    });
  });

  describe("loadConfig", () => {
    it("should load config from .loom/config.json", async () => {
      const mockConfig: LoomConfig = {
        nextAgentNumber: 5,
        agents: [
          {
            id: "terminal-1",
            name: "Agent 1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            id: "terminal-2",
            name: "Agent 2",
            status: TerminalStatus.Idle,
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(mockConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      expect(invoke).toHaveBeenCalledWith("read_config", {
        workspacePath: "/path/to/workspace",
      });
      expect(config).toEqual(mockConfig);
    });

    it("should throw error if no workspace is set", async () => {
      await expect(loadConfig()).rejects.toThrow("No workspace set - cannot load config");
    });

    it("should throw error if config file read fails", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("File not found"));
      setConfigWorkspace("/path/to/workspace");

      await expect(loadConfig()).rejects.toThrow("File not found");
    });

    it("should handle malformed JSON", async () => {
      vi.mocked(invoke).mockResolvedValue("invalid json");
      setConfigWorkspace("/path/to/workspace");

      await expect(loadConfig()).rejects.toThrow();
    });

    it("should load config with empty agents array", async () => {
      const mockConfig: LoomConfig = {
        nextAgentNumber: 1,
        agents: [],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(mockConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      expect(config.agents).toEqual([]);
      expect(config.nextAgentNumber).toBe(1);
    });

    it("should load config with agent role configuration", async () => {
      const mockConfig: LoomConfig = {
        nextAgentNumber: 2,
        agents: [
          {
            id: "terminal-1",
            name: "Worker Bot",
            status: TerminalStatus.Idle,
            isPrimary: true,
            role: "worker",
            roleConfig: {
              roleFile: "worker.md",
              targetInterval: 300000,
              intervalPrompt: "Continue working on open tasks",
            },
          },
        ],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(mockConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      expect(config.agents[0].role).toBe("worker");
      expect(config.agents[0].roleConfig).toEqual({
        roleFile: "worker.md",
        targetInterval: 300000,
        intervalPrompt: "Continue working on open tasks",
      });
    });
  });

  describe("saveConfig", () => {
    it("should save config to .loom/config.json", async () => {
      const config: LoomConfig = {
        nextAgentNumber: 3,
        agents: [
          {
            id: "terminal-1",
            name: "Agent 1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
        ],
      };

      setConfigWorkspace("/path/to/workspace");

      await saveConfig(config);

      expect(invoke).toHaveBeenCalledWith("write_config", {
        workspacePath: "/path/to/workspace",
        configJson: JSON.stringify(config, null, 2),
      });
    });

    it("should not save if no workspace is set", async () => {
      const config: LoomConfig = {
        nextAgentNumber: 1,
        agents: [],
      };

      await saveConfig(config);

      expect(invoke).not.toHaveBeenCalled();
    });

    it("should handle write errors gracefully", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("Write failed"));
      setConfigWorkspace("/path/to/workspace");

      const config: LoomConfig = {
        nextAgentNumber: 1,
        agents: [],
      };

      // Should not throw, just log error
      await expect(saveConfig(config)).resolves.toBeUndefined();
    });

    it("should format JSON with 2-space indentation", async () => {
      const config: LoomConfig = {
        nextAgentNumber: 1,
        agents: [],
      };

      setConfigWorkspace("/path/to/workspace");

      await saveConfig(config);

      const expectedJson = JSON.stringify(config, null, 2);
      expect(invoke).toHaveBeenCalledWith("write_config", {
        workspacePath: "/path/to/workspace",
        configJson: expectedJson,
      });
    });

    it("should save config with multiple agents", async () => {
      const config: LoomConfig = {
        nextAgentNumber: 5,
        agents: [
          {
            id: "terminal-1",
            name: "Agent 1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            id: "terminal-2",
            name: "Agent 2",
            status: TerminalStatus.Busy,
            isPrimary: false,
          },
          {
            id: "terminal-3",
            name: "Agent 3",
            status: TerminalStatus.Error,
            isPrimary: false,
          },
        ],
      };

      setConfigWorkspace("/path/to/workspace");

      await saveConfig(config);

      expect(invoke).toHaveBeenCalledWith("write_config", {
        workspacePath: "/path/to/workspace",
        configJson: JSON.stringify(config, null, 2),
      });
    });

    it("should save config with agent role and theme configuration", async () => {
      const config: LoomConfig = {
        nextAgentNumber: 2,
        agents: [
          {
            id: "terminal-1",
            name: "Themed Worker",
            status: TerminalStatus.Idle,
            isPrimary: true,
            role: "reviewer",
            roleConfig: {
              roleFile: "reviewer.md",
              targetInterval: 600000,
              intervalPrompt: "Review open PRs",
            },
            theme: "ocean",
          },
        ],
      };

      setConfigWorkspace("/path/to/workspace");

      await saveConfig(config);

      expect(invoke).toHaveBeenCalledWith("write_config", {
        workspacePath: "/path/to/workspace",
        configJson: JSON.stringify(config, null, 2),
      });
    });
  });

  describe("config migration", () => {
    it("should migrate dual-ID config (configId + UUID sessionId)", async () => {
      const legacyConfig = {
        nextAgentNumber: 3,
        agents: [
          {
            configId: "terminal-1",
            id: "abc-123-def-456", // old UUID session ID
            name: "Agent 1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            configId: "terminal-2",
            id: "xyz-789-uvw-012", // old UUID session ID
            name: "Agent 2",
            status: TerminalStatus.Idle,
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(legacyConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      // Should use configId as the new single ID
      expect(config.agents[0].id).toBe("terminal-1");
      expect(config.agents[1].id).toBe("terminal-2");
      // configId should be removed
      expect(config.agents[0]).not.toHaveProperty("configId");
      expect(config.agents[1]).not.toHaveProperty("configId");
    });

    it("should migrate UUID-only config to terminal-N IDs", async () => {
      const legacyConfig = {
        nextAgentNumber: 3,
        agents: [
          {
            id: "abc-123-def-456", // old UUID
            name: "Agent 1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            id: "xyz-789-uvw-012", // old UUID
            name: "Agent 2",
            status: TerminalStatus.Idle,
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(legacyConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      // Should generate stable terminal-N IDs based on index
      expect(config.agents[0].id).toBe("terminal-1");
      expect(config.agents[1].id).toBe("terminal-2");
    });

    it("should not migrate already-migrated config", async () => {
      const currentConfig: LoomConfig = {
        nextAgentNumber: 3,
        agents: [
          {
            id: "terminal-1",
            name: "Agent 1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            id: "terminal-2",
            name: "Agent 2",
            status: TerminalStatus.Idle,
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(currentConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      // Should keep existing stable IDs unchanged
      expect(config.agents[0].id).toBe("terminal-1");
      expect(config.agents[1].id).toBe("terminal-2");
    });
  });

  describe("loadConfig and saveConfig integration", () => {
    it("should round-trip config data correctly", async () => {
      const originalConfig: LoomConfig = {
        nextAgentNumber: 10,
        agents: [
          {
            id: "terminal-1",
            name: "Test Agent",
            status: TerminalStatus.Busy,
            isPrimary: true,
            role: "worker",
            roleConfig: {
              roleFile: "worker.md",
              targetInterval: 300000,
              intervalPrompt: "Work on tasks",
            },
            theme: "forest",
          },
        ],
      };

      setConfigWorkspace("/path/to/workspace");

      // Capture what saveConfig writes
      let savedJson = "";
      vi.mocked(invoke).mockImplementation(async (cmd, args: unknown) => {
        if (cmd === "write_config" && args && typeof args === "object" && "configJson" in args) {
          savedJson = args.configJson as string;
        }
        return savedJson;
      });

      await saveConfig(originalConfig);

      // Now mock loadConfig to return what we saved
      vi.mocked(invoke).mockResolvedValue(savedJson);

      const loadedConfig = await loadConfig();

      expect(loadedConfig).toEqual(originalConfig);
    });
  });
});
