import type { Page } from "@playwright/test";
import { selectors } from "./selectors";

/**
 * Test setup utilities for Loom E2E tests.
 */

/**
 * Wait for the app to fully load and become interactive.
 * Uses multiple strategies to ensure the app is ready.
 */
export async function waitForAppReady(page: Page): Promise<void> {
  // Wait for the main app container to be present
  await page.waitForSelector(selectors.app, {
    state: "attached",
    timeout: 15000,
  });

  // Wait for initial loading to complete
  await page.waitForLoadState("networkidle");

  // Give the app a moment to finish any async initialization
  await page.waitForTimeout(500);
}

/**
 * Mock the Tauri API for testing outside of Tauri context.
 * This allows tests to run in a regular browser.
 */
export async function mockTauriAPI(page: Page): Promise<void> {
  await page.addInitScript(() => {
    // Mock window.__TAURI__ for browser testing
    const mockTauri = {
      dialog: {
        open: async (options: { directory?: boolean }) => {
          // Return a mock workspace path
          if (options?.directory) {
            return "/mock/workspace/path";
          }
          return "/mock/file/path";
        },
        confirm: async () => true,
        message: async () => {},
        ask: async () => true,
      },
      fs: {
        readTextFile: async (path: string) => {
          if (path.endsWith("config.json")) {
            return JSON.stringify({
              version: "2",
              nextAgentNumber: 2,
              terminals: [
                {
                  id: "terminal-1",
                  name: "Test Terminal",
                  role: "builder",
                  roleConfig: {
                    workerType: "claude",
                    roleFile: "builder.md",
                    targetInterval: 0,
                    intervalPrompt: "",
                  },
                },
              ],
            });
          }
          if (path.endsWith("state.json")) {
            return JSON.stringify({
              nextAgentNumber: 2,
              terminals: [
                {
                  id: "terminal-1",
                  status: "idle",
                  isPrimary: true,
                },
              ],
            });
          }
          return "";
        },
        writeTextFile: async () => {},
        exists: async () => true,
        createDir: async () => {},
        readDir: async () => [],
      },
      path: {
        join: async (...parts: string[]) => parts.join("/"),
        basename: async (p: string) => p.split("/").pop() || "",
        dirname: async (p: string) => p.split("/").slice(0, -1).join("/") || "/",
        resolve: async (...parts: string[]) => parts.join("/"),
      },
      invoke: async (cmd: string, _args?: unknown) => {
        // Return appropriate mock responses based on command
        switch (cmd) {
          case "get_terminals":
            return [
              {
                id: "terminal-1",
                name: "Test Terminal",
                status: "idle",
                isPrimary: true,
                role: "builder",
              },
            ];
          case "get_terminal_output":
            return "";
          case "get_workspace_path":
            return "/mock/workspace/path";
          case "check_daemon_connection":
            return { connected: false };
          case "check_system_dependencies":
            // Return all dependencies available
            return {
              tmux_available: true,
              git_available: true,
              claude_code_available: true,
              gh_available: true,
              gh_copilot_available: true,
              gemini_cli_available: false,
              deepseek_cli_available: false,
              grok_cli_available: false,
              amp_cli_available: false,
            };
          case "get_cli_workspace":
            return null; // No CLI workspace argument
          case "get_stored_workspace":
            return null; // No stored workspace
          case "clear_stored_workspace":
            return null;
          case "store_workspace":
            return null;
          case "set_stored_workspace":
            return null;
          case "validate_workspace_path":
            return true; // All paths are valid in tests
          case "check_loom_initialized":
            return true; // Workspace is already initialized
          case "ensure_workspace_scaffolding":
            return null;
          case "read_config":
            return JSON.stringify({
              version: "2",
              terminals: [
                {
                  id: "terminal-1",
                  name: "Test Terminal",
                  role: "builder",
                  roleConfig: {
                    workerType: "claude",
                    roleFile: "builder.md",
                    targetInterval: 0,
                    intervalPrompt: "",
                  },
                },
              ],
            });
          case "read_state":
            return JSON.stringify({
              nextAgentNumber: 2,
              terminals: [
                {
                  id: "terminal-1",
                  status: "idle",
                  isPrimary: true,
                },
              ],
            });
          case "create_terminal":
            return "terminal-1"; // Return session ID
          case "save_config":
            return null;
          case "save_state":
            return null;
          case "get_daemon_status":
            return { running: false, socket_path: "", error: null };
          default:
            return null;
        }
      },
      event: {
        listen: async (_event: string, _handler: (event: unknown) => void) => {
          // Return an unsubscribe function
          return () => {};
        },
        emit: async () => {},
        once: async () => () => {},
      },
      window: {
        getCurrent: () => ({
          listen: async () => () => {},
          emit: async () => {},
          close: async () => {},
          setTitle: async () => {},
        }),
      },
      app: {
        getName: async () => "Loom",
        getVersion: async () => "1.0.0",
      },
      os: {
        platform: async () => "darwin",
        arch: async () => "x64",
      },
    };

    // Set up the mock
    (window as Window & { __TAURI__?: unknown }).__TAURI__ = mockTauri;

    // Also set up @tauri-apps/api module mocks
    (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {
      invoke: mockTauri.invoke,
      transformCallback: () => {},
    };
  });
}

/**
 * Skip workspace selection for tests that need an already-selected workspace.
 */
export async function selectMockWorkspace(page: Page): Promise<void> {
  await page.evaluate(() => {
    localStorage.setItem("loom:workspace", "/mock/workspace/path");
  });
  await page.reload();
  await waitForAppReady(page);
}

/**
 * Clear all persisted state between tests.
 */
export async function clearPersistedState(page: Page): Promise<void> {
  await page.evaluate(() => {
    localStorage.clear();
    sessionStorage.clear();
  });
}

/**
 * Check if the terminal is visible in single-session UI.
 */
export async function isTerminalVisible(page: Page): Promise<boolean> {
  const wrapper = page.locator(selectors.terminalContainer);
  return await wrapper.isVisible();
}

/**
 * Open terminal settings modal via the settings button.
 */
export async function openTerminalSettings(page: Page): Promise<void> {
  await page.click(selectors.terminalSettingsBtn);

  // Wait for settings modal
  await page.waitForSelector(selectors.settingsModal, { timeout: 5000 });
}

/**
 * Configure terminal settings.
 */
export async function configureTerminal(
  page: Page,
  options?: { role?: string; workerType?: string }
): Promise<void> {
  await openTerminalSettings(page);

  if (options?.role) {
    await page.selectOption(selectors.roleSelect, options.role);
  }

  if (options?.workerType) {
    await page.selectOption(selectors.workerTypeSelect, options.workerType);
  }

  await page.click(selectors.saveSettingsBtn);

  // Wait for modal to close
  await page.waitForSelector(selectors.settingsModal, { state: "hidden" });
}

/**
 * Close the terminal via the close button.
 */
export async function closeTerminal(page: Page): Promise<void> {
  await page.click(selectors.terminalCloseBtn);

  // Wait for confirmation dialog and confirm
  await page.click(selectors.confirmResetBtn);
}
