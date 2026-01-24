import type { Page } from "@playwright/test";
import { selectors } from "./selectors";

/**
 * Test setup utilities for Loom E2E tests.
 */

/**
 * Wait for the app to fully load and become interactive.
 */
export async function waitForAppReady(page: Page): Promise<void> {
	// Wait for the main app container to be present
	await page.waitForSelector("#app", { state: "attached" });

	// Wait for initial loading to complete
	await page.waitForLoadState("networkidle");
}

/**
 * Mock the Tauri API for testing outside of Tauri context.
 * This allows tests to run in a regular browser.
 */
export async function mockTauriAPI(page: Page): Promise<void> {
	await page.addInitScript(() => {
		// Mock window.__TAURI__ for browser testing
		(window as Window & { __TAURI__?: unknown }).__TAURI__ = {
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
			},
			path: {
				join: async (...parts: string[]) => parts.join("/"),
				basename: async (p: string) => p.split("/").pop() || "",
			},
			invoke: async (cmd: string) => {
				console.log(`[Mock Tauri] invoke: ${cmd}`);
				return null;
			},
			event: {
				listen: async () => () => {},
				emit: async () => {},
			},
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
	options?: { role?: string; workerType?: string },
): Promise<void> {
	await page.click(selectors.addTerminalBtn);

	// Wait for settings modal
	await page.waitForSelector(selectors.settingsModal);

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
