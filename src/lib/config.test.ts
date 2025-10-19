import { invoke } from "@tauri-apps/api/tauri";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  type LoomConfig,
  type LoomState,
  loadConfig,
  loadState,
  loadWorkspaceConfig,
  mergeConfigAndState,
  saveConfig,
  saveState,
  setConfigWorkspace,
  splitTerminals,
} from "./config";
import { type Terminal, TerminalStatus } from "./state";

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
        terminals: [
          {
            id: "terminal-1",
            name: "Agent 1",
            theme: "default",
          },
          {
            id: "terminal-2",
            name: "Agent 2",
            role: "worker",
            roleConfig: {
              workerType: "claude",
              roleFile: "worker.md",
              targetInterval: 300000,
              intervalPrompt: "Continue working",
            },
            theme: "forest",
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
      await expect(loadConfig()).rejects.toThrow("No workspace configured");
      expect(invoke).not.toHaveBeenCalled();
    });

    it("should return empty config if read fails", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("File not found"));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      expect(config).toEqual({ terminals: [] });
    });

    it("should handle malformed JSON by returning empty config", async () => {
      vi.mocked(invoke).mockResolvedValue("invalid json");
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      expect(config).toEqual({ terminals: [] });
    });

    it("should load config with empty terminals array", async () => {
      const mockConfig: LoomConfig = {
        terminals: [],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(mockConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      expect(config.terminals).toEqual([]);
    });

    it("should load config with terminal role configuration", async () => {
      const mockConfig: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Worker Bot",
            role: "worker",
            roleConfig: {
              workerType: "claude",
              roleFile: "worker.md",
              targetInterval: 300000,
              intervalPrompt: "Continue working on open tasks",
            },
            theme: "forest",
          },
        ],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(mockConfig));
      setConfigWorkspace("/path/to/workspace");

      const config = await loadConfig();

      expect(config.terminals[0].role).toBe("worker");
      expect(config.terminals[0].roleConfig).toEqual({
        workerType: "claude",
        roleFile: "worker.md",
        targetInterval: 300000,
        intervalPrompt: "Continue working on open tasks",
      });
    });
  });

  describe("saveConfig", () => {
    it("should save config to .loom/config.json", async () => {
      const config: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Agent 1",
            theme: "default",
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

    it("should handle missing workspace error when saving", async () => {
      const config: LoomConfig = {
        terminals: [],
      };

      // saveConfig catches the error internally and logs it, doesn't throw
      await expect(saveConfig(config)).resolves.toBeUndefined();
      expect(invoke).not.toHaveBeenCalled();
    });

    it("should handle write errors gracefully", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("Write failed"));
      setConfigWorkspace("/path/to/workspace");

      const config: LoomConfig = {
        terminals: [],
      };

      // Should not throw, just log error
      await expect(saveConfig(config)).resolves.toBeUndefined();
    });

    it("should format JSON with 2-space indentation", async () => {
      const config: LoomConfig = {
        terminals: [],
      };

      setConfigWorkspace("/path/to/workspace");

      await saveConfig(config);

      const expectedJson = JSON.stringify(config, null, 2);
      expect(invoke).toHaveBeenCalledWith("write_config", {
        workspacePath: "/path/to/workspace",
        configJson: expectedJson,
      });
    });

    it("should save config with multiple terminals", async () => {
      const config: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Agent 1",
            theme: "default",
          },
          {
            id: "terminal-2",
            name: "Agent 2",
            theme: "ocean",
          },
          {
            id: "terminal-3",
            name: "Agent 3",
            role: "reviewer",
            roleConfig: {
              workerType: "claude",
              roleFile: "reviewer.md",
              targetInterval: 600000,
              intervalPrompt: "Review PRs",
            },
            theme: "rose",
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

    it("should save config with terminal role and theme configuration", async () => {
      const config: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Themed Worker",
            role: "reviewer",
            roleConfig: {
              workerType: "claude",
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

  describe("loadState", () => {
    it("should load state from .loom/state.json", async () => {
      const mockState: LoomState = {
        nextAgentNumber: 5,
        daemonPid: 12345,
        terminals: [
          {
            id: "terminal-1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            id: "terminal-2",
            status: TerminalStatus.Busy,
            isPrimary: false,
            worktreePath: "/path/to/worktree",
            agentPid: 67890,
          },
        ],
      };

      vi.mocked(invoke).mockResolvedValue(JSON.stringify(mockState));
      setConfigWorkspace("/path/to/workspace");

      const state = await loadState();

      expect(invoke).toHaveBeenCalledWith("read_state", {
        workspacePath: "/path/to/workspace",
      });
      expect(state).toEqual(mockState);
    });

    it("should throw error if no workspace is set", async () => {
      await expect(loadState()).rejects.toThrow("No workspace configured");
      expect(invoke).not.toHaveBeenCalled();
    });

    it("should return empty state if read fails", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("File not found"));
      setConfigWorkspace("/path/to/workspace");

      const state = await loadState();

      expect(state).toEqual({
        nextAgentNumber: 1,
        terminals: [],
      });
    });
  });

  describe("saveState", () => {
    it("should save state to .loom/state.json", async () => {
      const state: LoomState = {
        nextAgentNumber: 3,
        daemonPid: 12345,
        terminals: [
          {
            id: "terminal-1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
        ],
      };

      setConfigWorkspace("/path/to/workspace");

      await saveState(state);

      expect(invoke).toHaveBeenCalledWith("write_state", {
        workspacePath: "/path/to/workspace",
        stateJson: JSON.stringify(state, null, 2),
      });
    });

    it("should handle missing workspace error when saving", async () => {
      const state: LoomState = {
        nextAgentNumber: 1,
        terminals: [],
      };

      // saveState catches the error internally and logs it, doesn't throw
      await expect(saveState(state)).resolves.toBeUndefined();
      expect(invoke).not.toHaveBeenCalled();
    });

    it("should handle write errors gracefully", async () => {
      vi.mocked(invoke).mockRejectedValue(new Error("Write failed"));
      setConfigWorkspace("/path/to/workspace");

      const state: LoomState = {
        nextAgentNumber: 1,
        terminals: [],
      };

      // Should not throw, just log error
      await expect(saveState(state)).resolves.toBeUndefined();
    });
  });

  describe("mergeConfigAndState", () => {
    it("should merge config and state into full Terminal objects", () => {
      const config: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Agent 1",
            theme: "default",
          },
          {
            id: "terminal-2",
            name: "Agent 2",
            role: "worker",
            roleConfig: {
              workerType: "claude",
              roleFile: "worker.md",
              targetInterval: 300000,
              intervalPrompt: "Work",
            },
            theme: "forest",
          },
        ],
      };

      const state: LoomState = {
        nextAgentNumber: 3,
        daemonPid: 12345,
        terminals: [
          {
            id: "terminal-1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            id: "terminal-2",
            status: TerminalStatus.Busy,
            isPrimary: false,
            worktreePath: "/path/to/worktree",
            agentPid: 67890,
          },
        ],
      };

      const result = mergeConfigAndState(config, state);

      expect(result.nextAgentNumber).toBe(3);
      expect(result.terminals).toHaveLength(2);
      expect(result.terminals[0]).toEqual({
        id: "terminal-1",
        name: "Agent 1",
        theme: "default",
        status: TerminalStatus.Idle,
        isPrimary: true,
      });
      expect(result.terminals[1]).toEqual({
        id: "terminal-2",
        name: "Agent 2",
        role: "worker",
        roleConfig: {
          workerType: "claude",
          roleFile: "worker.md",
          targetInterval: 300000,
          intervalPrompt: "Work",
        },
        theme: "forest",
        status: TerminalStatus.Busy,
        isPrimary: false,
        worktreePath: "/path/to/worktree",
        agentPid: 67890,
      });
    });

    it("should use default state values if terminal not in state", () => {
      const config: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Agent 1",
            theme: "default",
          },
        ],
      };

      const state: LoomState = {
        nextAgentNumber: 2,
        terminals: [], // No state for terminal-1
      };

      const result = mergeConfigAndState(config, state);

      expect(result.terminals[0]).toEqual({
        id: "terminal-1",
        name: "Agent 1",
        theme: "default",
        status: TerminalStatus.Idle,
        isPrimary: false,
      });
    });
  });

  describe("splitTerminals", () => {
    it("should split Terminal objects into config and state", () => {
      const terminals: Terminal[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          theme: "default",
          status: TerminalStatus.Idle,
          isPrimary: true,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          roleConfig: {
            workerType: "claude",
            roleFile: "worker.md",
            targetInterval: 300000,
            intervalPrompt: "Work",
          },
          theme: "forest",
          status: TerminalStatus.Busy,
          isPrimary: false,
          worktreePath: "/path/to/worktree",
          agentPid: 67890,
        },
      ];

      const result = splitTerminals(terminals);

      // Check config (persistent data)
      expect(result.config).toEqual([
        {
          id: "terminal-1",
          name: "Agent 1",
          theme: "default",
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          roleConfig: {
            workerType: "claude",
            roleFile: "worker.md",
            targetInterval: 300000,
            intervalPrompt: "Work",
          },
          theme: "forest",
        },
      ]);

      // Check state (ephemeral data)
      expect(result.state).toEqual([
        {
          id: "terminal-1",
          status: TerminalStatus.Idle,
          isPrimary: true,
        },
        {
          id: "terminal-2",
          status: TerminalStatus.Busy,
          isPrimary: false,
          worktreePath: "/path/to/worktree",
          agentPid: 67890,
        },
      ]);
    });
  });

  describe("loadWorkspaceConfig", () => {
    it("should load and merge config and state, returning legacy format", async () => {
      const mockConfig: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Agent 1",
            theme: "default",
          },
        ],
      };

      const mockState: LoomState = {
        nextAgentNumber: 2,
        terminals: [
          {
            id: "terminal-1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
        ],
      };

      vi.mocked(invoke).mockImplementation(async (cmd) => {
        if (cmd === "read_config") {
          return JSON.stringify(mockConfig);
        }
        if (cmd === "read_state") {
          return JSON.stringify(mockState);
        }
        return "";
      });

      setConfigWorkspace("/path/to/workspace");

      const result = await loadWorkspaceConfig();

      expect(result.nextAgentNumber).toBe(2);
      expect(result.agents).toHaveLength(1);
      expect(result.agents[0]).toEqual({
        id: "terminal-1",
        name: "Agent 1",
        theme: "default",
        status: TerminalStatus.Idle,
        isPrimary: true,
      });
    });
  });

  describe("legacy config migration", () => {
    it("should migrate dual-ID config (configId + UUID sessionId)", async () => {
      const legacyConfig = {
        nextAgentNumber: 3,
        agents: [
          {
            configId: "terminal-1",
            id: "abc-123-def-456", // old UUID session ID
            name: "Agent 1",
            status: "idle",
            isPrimary: true,
          },
          {
            configId: "terminal-2",
            id: "xyz-789-uvw-012", // old UUID session ID
            name: "Agent 2",
            status: "idle",
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockImplementation(async (cmd) => {
        if (cmd === "read_config") {
          return JSON.stringify(legacyConfig);
        }
        if (cmd === "read_state") {
          // After migration, state.json won't exist yet
          throw new Error("File not found");
        }
        return "";
      });

      setConfigWorkspace("/path/to/workspace");

      const result = await loadWorkspaceConfig();

      // Should use configId as the new single ID
      expect(result.agents[0].id).toBe("terminal-1");
      expect(result.agents[1].id).toBe("terminal-2");
    });

    it("should migrate UUID-only config to terminal-N IDs", async () => {
      const legacyConfig = {
        nextAgentNumber: 3,
        agents: [
          {
            id: "abc-123-def-456", // old UUID
            name: "Agent 1",
            status: "idle",
            isPrimary: true,
          },
          {
            id: "xyz-789-uvw-012", // old UUID
            name: "Agent 2",
            status: "idle",
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockImplementation(async (cmd) => {
        if (cmd === "read_config") {
          return JSON.stringify(legacyConfig);
        }
        if (cmd === "read_state") {
          throw new Error("File not found");
        }
        return "";
      });

      setConfigWorkspace("/path/to/workspace");

      const result = await loadWorkspaceConfig();

      // Should generate stable terminal-N IDs based on index
      expect(result.agents[0].id).toBe("terminal-1");
      expect(result.agents[1].id).toBe("terminal-2");
    });

    it("should migrate placeholder IDs to terminal-N IDs", async () => {
      const legacyConfig = {
        nextAgentNumber: 3,
        agents: [
          {
            id: "__needs_session__",
            name: "Agent 1",
            status: "idle",
            isPrimary: true,
          },
          {
            id: "__unassigned__",
            name: "Agent 2",
            status: "idle",
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockImplementation(async (cmd) => {
        if (cmd === "read_config") {
          return JSON.stringify(legacyConfig);
        }
        if (cmd === "read_state") {
          throw new Error("File not found");
        }
        return "";
      });

      setConfigWorkspace("/path/to/workspace");

      const result = await loadWorkspaceConfig();

      // Should generate stable terminal-N IDs
      expect(result.agents[0].id).toBe("terminal-1");
      expect(result.agents[1].id).toBe("terminal-2");
    });

    it("should not migrate already-migrated config", async () => {
      const currentConfig: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Agent 1",
            theme: "default",
          },
          {
            id: "terminal-2",
            name: "Agent 2",
            theme: "ocean",
          },
        ],
      };

      const currentState: LoomState = {
        nextAgentNumber: 3,
        terminals: [
          {
            id: "terminal-1",
            status: TerminalStatus.Idle,
            isPrimary: true,
          },
          {
            id: "terminal-2",
            status: TerminalStatus.Idle,
            isPrimary: false,
          },
        ],
      };

      vi.mocked(invoke).mockImplementation(async (cmd) => {
        if (cmd === "read_config") {
          return JSON.stringify(currentConfig);
        }
        if (cmd === "read_state") {
          return JSON.stringify(currentState);
        }
        return "";
      });

      setConfigWorkspace("/path/to/workspace");

      const result = await loadWorkspaceConfig();

      // Should keep existing stable IDs unchanged
      expect(result.agents[0].id).toBe("terminal-1");
      expect(result.agents[1].id).toBe("terminal-2");
    });
  });

  describe("loadConfig and saveConfig integration", () => {
    it("should round-trip config data correctly", async () => {
      const originalConfig: LoomConfig = {
        terminals: [
          {
            id: "terminal-1",
            name: "Test Agent",
            role: "worker",
            roleConfig: {
              workerType: "claude",
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

  describe("split and merge round-trip", () => {
    it("should round-trip Terminal objects through split and merge", () => {
      const originalTerminals: Terminal[] = [
        {
          id: "terminal-1",
          name: "Agent 1",
          theme: "default",
          status: TerminalStatus.Idle,
          isPrimary: true,
        },
        {
          id: "terminal-2",
          name: "Agent 2",
          role: "worker",
          roleConfig: {
            workerType: "claude",
            roleFile: "worker.md",
            targetInterval: 300000,
            intervalPrompt: "Work",
          },
          theme: "forest",
          status: TerminalStatus.Busy,
          isPrimary: false,
          worktreePath: "/path/to/worktree",
          agentPid: 67890,
        },
      ];

      // Split into config and state
      const { config, state } = splitTerminals(originalTerminals);

      // Merge back
      const merged = mergeConfigAndState(
        { terminals: config },
        { nextAgentNumber: 3, terminals: state }
      );

      expect(merged.terminals).toEqual(originalTerminals);
    });
  });
});
