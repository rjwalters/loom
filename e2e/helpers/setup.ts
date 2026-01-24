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
              nextAgentNumber: 1,
              terminals: [],
            });
          }
          if (path.endsWith("state.json")) {
            return JSON.stringify({
              workspace: "/mock/workspace/path",
              engineRunning: false,
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
            return [];
          case "get_terminal_output":
            return "";
          case "get_workspace_path":
            return "/mock/workspace/path";
          case "check_daemon_connection":
            return { connected: false };
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
 * Get terminal count from the UI.
 */
export async function getTerminalCount(page: Page): Promise<number> {
  return await page.locator(selectors.terminalCard).count();
}

/**
 * Create a new terminal via the UI.
 */
export async function createTerminal(
  page: Page,
  options?: { role?: string; workerType?: string }
): Promise<void> {
  await page.click(selectors.addTerminalBtn);

  // Wait for settings modal
  await page.waitForSelector(selectors.settingsModal, { timeout: 5000 });

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
 * Delete a terminal by index.
 */
export async function deleteTerminal(page: Page, index = 0): Promise<void> {
  const terminal = page.locator(selectors.terminalCard).nth(index);

  // Hover to reveal delete button
  await terminal.hover();
  await terminal.locator(selectors.terminalDeleteBtn).click();

  // Confirm deletion
  await page.click(selectors.confirmResetBtn);
}
