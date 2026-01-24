import { expect, test } from "@playwright/test";
import { mockTauriAPI, waitForAppReady } from "./helpers/setup";

test.describe("App Launch", () => {
	test.beforeEach(async ({ page }) => {
		await mockTauriAPI(page);
	});

	test("should load the application", async ({ page }) => {
		await page.goto("/");
		await waitForAppReady(page);

		// App should render without errors
		await expect(page.locator("#app")).toBeVisible();
	});

	test("should show workspace picker on first launch", async ({ page }) => {
		// Clear any persisted workspace
		await page.goto("/");
		await page.evaluate(() => localStorage.clear());
		await page.reload();
		await waitForAppReady(page);

		// Should prompt for workspace selection
		// Note: The exact selector depends on implementation
		const hasWorkspaceUI =
			(await page.locator("text=Select Workspace").count()) > 0 ||
			(await page.locator("text=Choose Workspace").count()) > 0 ||
			(await page.locator("text=Open Workspace").count()) > 0;

		expect(hasWorkspaceUI).toBeTruthy();
	});

	test("should have no console errors on launch", async ({ page }) => {
		const consoleErrors: string[] = [];

		page.on("console", (msg) => {
			if (msg.type() === "error") {
				consoleErrors.push(msg.text());
			}
		});

		await page.goto("/");
		await waitForAppReady(page);

		// Filter out expected errors (e.g., Tauri not available in browser)
		const unexpectedErrors = consoleErrors.filter(
			(err) =>
				!err.includes("__TAURI__") &&
				!err.includes("Tauri") &&
				!err.includes("Failed to fetch"),
		);

		expect(unexpectedErrors).toHaveLength(0);
	});

	test("should be responsive at different viewport sizes", async ({
		page,
	}) => {
		await page.goto("/");
		await waitForAppReady(page);

		// Test minimum viewport
		await page.setViewportSize({ width: 1000, height: 600 });
		await expect(page.locator("#app")).toBeVisible();

		// Test large viewport
		await page.setViewportSize({ width: 1920, height: 1080 });
		await expect(page.locator("#app")).toBeVisible();
	});
});
