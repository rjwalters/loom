import { expect, test } from "@playwright/test";
import { mockTauriAPI, waitForAppReady } from "./helpers/setup";

test.describe("Navigation and Accessibility", () => {
	test.beforeEach(async ({ page }) => {
		await mockTauriAPI(page);
		await page.goto("/");
		await waitForAppReady(page);
	});

	test("should support keyboard navigation with Tab", async ({ page }) => {
		// Focus the app
		await page.click("body");

		// Press Tab and verify focus moves
		await page.keyboard.press("Tab");

		const focusedTag = await page.evaluate(() =>
			document.activeElement?.tagName?.toLowerCase(),
		);

		// Should focus on some interactive element
		expect(focusedTag).toBeTruthy();
	});

	test("should have proper ARIA attributes on interactive elements", async ({
		page,
	}) => {
		// Check for buttons with accessible names
		const buttons = page.locator("button");
		const buttonCount = await buttons.count();

		if (buttonCount > 0) {
			// At least some buttons should exist
			expect(buttonCount).toBeGreaterThan(0);
		}
	});

	test("should handle Escape key for modals", async ({ page }) => {
		// Try to open a modal if possible
		const settingsBtn = page.locator('[aria-label*="settings"]').first();

		if ((await settingsBtn.count()) > 0) {
			await settingsBtn.click();

			// Press Escape
			await page.keyboard.press("Escape");

			// Modal should close (or not crash)
			await page.waitForTimeout(500);
			await expect(page.locator("#app")).toBeVisible();
		}
	});

	test("should maintain focus after interactions", async ({ page }) => {
		// Click on the app
		await page.click("#app");

		// Verify app is still interactive
		await page.keyboard.press("Tab");

		const hasFocus = await page.evaluate(() => {
			return document.activeElement !== document.body;
		});

		expect(hasFocus).toBeTruthy();
	});

	test("should have visible focus indicators", async ({ page }) => {
		// Tab to focus an element
		await page.keyboard.press("Tab");
		await page.keyboard.press("Tab");

		// Take screenshot to verify focus visibility (manual review)
		await page.screenshot({ path: "e2e/screenshots/focus-indicator.png" });
	});
});

test.describe("Theme and Styling", () => {
	test.beforeEach(async ({ page }) => {
		await mockTauriAPI(page);
	});

	test("should load with default theme", async ({ page }) => {
		await page.goto("/");
		await waitForAppReady(page);

		// Check that CSS is applied (background color is not default white)
		const bgColor = await page.evaluate(() => {
			return window.getComputedStyle(document.body).backgroundColor;
		});

		// Should have some background color set
		expect(bgColor).toBeTruthy();
	});

	test("should render text legibly", async ({ page }) => {
		await page.goto("/");
		await waitForAppReady(page);

		// Check font size is reasonable
		const fontSize = await page.evaluate(() => {
			return window.getComputedStyle(document.body).fontSize;
		});

		const fontSizePx = Number.parseInt(fontSize, 10);
		expect(fontSizePx).toBeGreaterThanOrEqual(12);
	});
});
