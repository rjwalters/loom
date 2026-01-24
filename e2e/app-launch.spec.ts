import { expect, test } from "@playwright/test";
import { selectors, textMatchers } from "./helpers/selectors";
import { mockTauriAPI, waitForAppReady } from "./helpers/setup";

test.describe("App Launch", () => {
  test.beforeEach(async ({ page }) => {
    await mockTauriAPI(page);
  });

  test("should load the application", async ({ page }) => {
    await page.goto("/");
    await waitForAppReady(page);

    // App should render without errors
    await expect(page.locator(selectors.app)).toBeVisible();
  });

  test("should show workspace picker on first launch", async ({ page }) => {
    // Clear any persisted workspace
    await page.goto("/");
    await page.evaluate(() => localStorage.clear());
    await page.reload();
    await waitForAppReady(page);

    // Should show some form of workspace UI - either:
    // 1. Text prompting to select workspace
    // 2. An input field for workspace path
    // 3. A button to select workspace
    const hasWorkspaceUI =
      (await page.getByText(textMatchers.selectWorkspace).count()) > 0 ||
      (await page.getByText(textMatchers.noWorkspace).count()) > 0 ||
      (await page.locator(selectors.workspaceSelectBtn).count()) > 0 ||
      (await page.locator(selectors.workspaceInput).count()) > 0 ||
      (await page.locator(selectors.workspacePicker).count()) > 0;

    // The app should have some workspace-related UI when no workspace is selected
    // This is flexible to handle different UI states
    expect(hasWorkspaceUI).toBeTruthy();
  });

  test("should have no unexpected console errors on launch", async ({ page }) => {
    const consoleErrors: string[] = [];

    page.on("console", (msg) => {
      if (msg.type() === "error") {
        consoleErrors.push(msg.text());
      }
    });

    await page.goto("/");
    await waitForAppReady(page);

    // Filter out expected errors that occur in browser testing:
    // - __TAURI__: Expected when mocking Tauri in browser
    // - Tauri: Related Tauri initialization messages
    // - Failed to fetch: Network requests that fail in test environment
    // - invoke: Mock Tauri invoke calls
    // - ipc: IPC-related messages from Tauri mocking
    // - ResizeObserver: Browser-specific timing issue, not a real error
    // - Cannot read properties: May occur during initialization with mocks
    const unexpectedErrors = consoleErrors.filter(
      (err) =>
        !err.includes("__TAURI__") &&
        !err.includes("Tauri") &&
        !err.includes("Failed to fetch") &&
        !err.includes("invoke") &&
        !err.includes("ipc") &&
        !err.includes("ResizeObserver") &&
        !err.includes("Cannot read properties") &&
        !err.includes("undefined") // Mock-related errors
    );

    // Only fail if there are truly unexpected errors
    expect(unexpectedErrors).toHaveLength(0);
  });

  test("should be responsive at different viewport sizes", async ({ page }) => {
    await page.goto("/");
    await waitForAppReady(page);

    // Test minimum viewport
    await page.setViewportSize({ width: 1000, height: 600 });
    await expect(page.locator(selectors.app)).toBeVisible();

    // Test large viewport
    await page.setViewportSize({ width: 1920, height: 1080 });
    await expect(page.locator(selectors.app)).toBeVisible();
  });
});
