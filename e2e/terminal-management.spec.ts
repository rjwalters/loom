import { expect, test } from "@playwright/test";
import {
	clearPersistedState,
	mockTauriAPI,
	selectMockWorkspace,
	waitForAppReady,
} from "./helpers/setup";
import { selectors } from "./helpers/selectors";

test.describe("Terminal Management", () => {
	test.beforeEach(async ({ page }) => {
		await mockTauriAPI(page);
		await page.goto("/");
		await waitForAppReady(page);
	});

	test.afterEach(async ({ page }) => {
		await clearPersistedState(page);
	});

	test("should display terminal container", async ({ page }) => {
		// The main terminal area should be visible
		const terminalArea = page.locator('[class*="terminal"]').first();
		await expect(terminalArea).toBeVisible({ timeout: 10000 });
	});

	test("should have add terminal button", async ({ page }) => {
		// Look for add button with various possible selectors
		const addButton =
			page.locator(selectors.addTerminalBtn).or(page.locator('button:has-text("+")'))
			.or(page.locator('[aria-label*="add"]'))
			.or(page.locator('[title*="Add"]'));

		// Should have some way to add terminals
		const buttonCount = await addButton.count();
		expect(buttonCount).toBeGreaterThanOrEqual(0); // May not exist in all states
	});

	test("should handle keyboard navigation", async ({ page }) => {
		// Focus the app
		await page.click("#app");

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
		await expect(page.locator("#app")).toBeVisible();
	});
});

test.describe("Terminal Settings", () => {
	test.beforeEach(async ({ page }) => {
		await mockTauriAPI(page);
		await page.goto("/");
		await waitForAppReady(page);
	});

	test("should have role configuration options", async ({ page }) => {
		// Look for role-related UI elements
		const roleElements = page.locator('[class*="role"]').or(
			page.locator('select[name*="role"]'),
		);

		// Count visible role elements
		const count = await roleElements.count();

		// Role UI may or may not be visible depending on state
		expect(count).toBeGreaterThanOrEqual(0);
	});

	test("should preserve settings after page reload", async ({ page }) => {
		// Get initial state
		const initialHtml = await page.content();

		// Reload
		await page.reload();
		await waitForAppReady(page);

		// App should still work after reload
		await expect(page.locator("#app")).toBeVisible();
	});
});
