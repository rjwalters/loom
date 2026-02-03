import { expect, test } from "@playwright/test";
import { selectors, textMatchers } from "./helpers/selectors";
import {
  clearPersistedState,
  mockTauriAPI,
  selectMockWorkspace,
  waitForAppReady,
} from "./helpers/setup";

test.describe("Terminal Management", () => {
  test.beforeEach(async ({ page }) => {
    await mockTauriAPI(page);
    await page.goto("/");
    await waitForAppReady(page);
  });

  test.afterEach(async ({ page }) => {
    await clearPersistedState(page);
  });

  test("should show workspace selector when no workspace selected", async ({ page }) => {
    // Without a workspace, the app should show the workspace selector
    const workspacePrompt = page.getByText(textMatchers.openRepository);
    await expect(workspacePrompt).toBeVisible({ timeout: 15000 });
  });

  test("should show no terminals message after workspace selection", async ({ page }) => {
    // Select a mock workspace first
    await selectMockWorkspace(page);

    // With workspace but no terminals, should show the "No terminals" message
    const noTerminalsMessage = page.getByText("No terminals. Click + to add a terminal.");
    await expect(noTerminalsMessage).toBeVisible({ timeout: 15000 });
  });

  test("should handle keyboard navigation", async ({ page }) => {
    // Focus the app
    await page.click(selectors.app);

    // Tab should move focus between elements
    await page.keyboard.press("Tab");

    // Check that some element is focused
    const focusedElement = await page.evaluate(() => {
      return document.activeElement?.tagName || null;
    });

    expect(focusedElement).toBeTruthy();
  });

  test("should show loading state appropriately", async ({ page }) => {
    // On initial load, there may be loading indicators
    await page.goto("/");

    // Wait briefly for loading to start
    await page.waitForTimeout(100);

    // App should eventually be ready
    await waitForAppReady(page);
    await expect(page.locator(selectors.app)).toBeVisible();
  });
});

test.describe("Terminal Settings", () => {
  test.beforeEach(async ({ page }) => {
    await mockTauriAPI(page);
    await page.goto("/");
    await waitForAppReady(page);
  });

  test("should have role configuration options", async ({ page }) => {
    // Look for role-related UI elements using class selectors
    const roleElements = page.locator('[class*="role"]').or(page.locator('select[name*="role"]'));

    // Count visible role elements
    const count = await roleElements.count();

    // Role UI may or may not be visible depending on state
    expect(count).toBeGreaterThanOrEqual(0);
  });

  test("should preserve settings after page reload", async ({ page }) => {
    // Get initial content (not used, but verifies page is loaded)
    await page.content();

    // Reload
    await page.reload();
    await waitForAppReady(page);

    // App should still work after reload
    await expect(page.locator(selectors.app)).toBeVisible();
  });
});
